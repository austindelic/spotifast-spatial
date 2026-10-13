use std::error::Error;
use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::PathBuf;
use std::ptr::{NonNull, null, null_mut};
use std::time::{Duration, Instant};

use objc2::MainThreadMarker;
use objc2::rc::{Retained, autoreleasepool};
use objc2::runtime::ProtocolObject;
use objc2_app_kit::{NSApplication, NSApplicationActivationPolicy};
use objc2_av_foundation::{
    AVAudioSpatializationFormats, AVQueuedSampleBufferRendering,
    AVQueuedSampleBufferRenderingStatus, AVSampleBufferAudioRenderer,
    AVSampleBufferRenderSynchronizer,
};
use objc2_core_audio_types::{
    AudioStreamBasicDescription, kAudioFormatFlagIsFloat, kAudioFormatFlagIsPacked,
    kAudioFormatLinearPCM,
};
use objc2_core_foundation::CFRetained;
use objc2_core_media::{
    CMAudioFormatDescriptionCreate, CMBlockBuffer, CMFormatDescription, CMSampleBuffer,
    CMSampleTimingInfo, CMTime, kCMTimeInvalid,
};
use objc2_foundation::{NSBundle, NSDate, NSProcessInfo, NSRunLoop};

use crate::signal::{BLOCK_FRAMES, CHANNELS, SAMPLE_RATE, next_chunk, samples};

type Result<T> = std::result::Result<T, Box<dyn Error>>;
const IDENTIFIER: &str = "rocks.spotifast.Spotifast";

#[derive(Debug, PartialEq)]
struct Options {
    seconds: u64,
    check: bool,
    log: Option<PathBuf>,
}

impl Options {
    fn parse(args: impl IntoIterator<Item = String>) -> Result<Self> {
        let mut options = Self {
            seconds: 120,
            check: false,
            log: None,
        };
        let mut args = args.into_iter();
        while let Some(arg) = args.next() {
            match arg.as_str() {
                "--check" => options.check = true,
                "--seconds" => {
                    options.seconds = args.next().ok_or("--seconds needs a value")?.parse()?;
                    if !(1..=600).contains(&options.seconds) {
                        return Err("--seconds must be between 1 and 600".into());
                    }
                }
                "--log" => options.log = Some(args.next().ok_or("--log needs a path")?.into()),
                _ => {
                    return Err(
                        "Usage: spatial-audio-probe [--check] [--seconds 1..600] [--log PATH]"
                            .into(),
                    );
                }
            }
        }
        Ok(options)
    }
}

struct Log(Option<File>);

impl Log {
    fn write(&mut self, message: impl std::fmt::Display) -> Result<()> {
        println!("{message}");
        if let Some(file) = &mut self.0 {
            writeln!(file, "{message}")?;
            file.flush()?;
        }
        Ok(())
    }
}

pub fn run() -> Result<()> {
    let options = Options::parse(std::env::args().skip(1))?;
    let mut log = Log(options
        .log
        .as_ref()
        .map(|path| OpenOptions::new().create(true).append(true).open(path))
        .transpose()?);
    let result = autoreleasepool(|_| probe(&options, &mut log));
    if let Err(error) = &result {
        log.write(format!("FAILED: {error}"))?;
    }
    result
}

/// All Apple audio calls run serially on this thread. The probe installs no
/// data callbacks or notifications and has no Spotify session or UI state.
fn probe(options: &Options, log: &mut Log) -> Result<()> {
    let info = NSProcessInfo::processInfo();
    let version = info.operatingSystemVersion();
    log.write(format!(
        "Spatial audio feasibility probe, {}",
        std::env::consts::ARCH
    ))?;
    log.write(info.operatingSystemVersionString())?;
    if !supported(
        std::env::consts::ARCH,
        version.majorVersion,
        version.minorVersion,
    ) {
        return Err("Requires Apple silicon and macOS 12.3 or later".into());
    }
    let identity = NSBundle::mainBundle()
        .bundleIdentifier()
        .map(|id| id.to_string());
    log.write(format!(
        "Bundle identity: {}",
        identity.as_deref().unwrap_or("unbundled")
    ))?;
    if !options.check && identity.as_deref() != Some(IDENTIFIER) {
        return Err("Package with packaging/macos/bundle.sh before the listening test".into());
    }
    if !options.check {
        let mtm = MainThreadMarker::new().ok_or("Must launch on the main thread")?;
        let app = NSApplication::sharedApplication(mtm);
        app.setActivationPolicy(NSApplicationActivationPolicy::Regular);
        app.finishLaunching();
    }
    let format = format()?;
    let output = Audio::new();
    log.write("44,100 Hz, interleaved float32 stereo; mono/stereo spatialization permitted")?;
    // SAFETY: Owned renderer on this thread; public property available after
    // the macOS version check. This reports permission, not active spatialization.
    let allowed = unsafe { output.renderer.allowedAudioSpatializationFormats() };
    if allowed != AVAudioSpatializationFormats::MonoAndStereo {
        return Err("Renderer did not retain the requested spatialization formats".into());
    }
    if options.check {
        let buffer = sample_buffer(&format, 0, BLOCK_FRAMES)?;
        // SAFETY: Renderer is attached to its synchronizer, and buffer owns its
        // PCM. Rate remains zero, so this smoke check does not play audio.
        unsafe { output.renderer.enqueueSampleBuffer(&buffer) };
        output.check_status()?;
        return log.write(
            "CHECK PASSED: owned PCM buffer enqueued at rate zero. AirPods behavior NOT verified.",
        );
    }

    log.write(format!(
        "Listening test: {} seconds. Compare Off, Fixed, and Head Tracked in macOS.",
        options.seconds
    ))?;
    log.write("This generates quiet diagnostic tones, not Spotify playback. Device changes are outside this prototype.")?;
    let end = options.seconds * u64::from(SAMPLE_RATE);
    let mut next = 0;
    while let Some(count) = next_chunk(next, 0, end) {
        let buffer = sample_buffer(&format, next, count)?;
        // SAFETY: Attached renderer and fully initialized, owned PCM buffer.
        unsafe { output.renderer.enqueueSampleBuffer(&buffer) };
        next += count;
    }
    // SAFETY: Enqueued timestamps start at zero, and the synchronizer owns
    // the attached renderer. Every operation remains on this thread.
    unsafe {
        output
            .synchronizer
            .setRate_time(1.0, CMTime::new(0, SAMPLE_RATE as i32))
    };
    let deadline = Instant::now() + Duration::from_secs(options.seconds + 5);
    let mut underflows = 0;
    loop {
        output.check_status()?;
        // SAFETY: Owned synchronizer, read on the same thread as all changes.
        let seconds = unsafe { output.synchronizer.currentTime().seconds() };
        if !seconds.is_finite() || seconds < 0.0 {
            return Err("Invalid renderer playback clock".into());
        }
        let played = (seconds * f64::from(SAMPLE_RATE)).floor() as u64;
        if played >= end {
            break;
        }
        if Instant::now() >= deadline {
            return Err("Renderer playback clock stalled".into());
        }
        if played > next && next < end {
            // Do not enqueue expired timestamps after a late wakeup. Keep
            // counting missed frames; they invalidate a clean hardware result.
            underflows += 1;
            log.write(format!("UNDERRUN: missed {} frames", played - next))?;
            next = played.min(end);
        }
        while let Some(count) = next_chunk(next, played, end) {
            // SAFETY: Readiness is queried on the sole enqueueing thread.
            if !unsafe { output.renderer.isReadyForMoreMediaData() } {
                break;
            }
            let buffer = sample_buffer(&format, next, count)?;
            // SAFETY: Buffer has owned data and valid timestamps in the bounded horizon.
            unsafe { output.renderer.enqueueSampleBuffer(&buffer) };
            next += count;
        }
        autoreleasepool(|_| {
            NSRunLoop::currentRunLoop().runUntilDate(&NSDate::dateWithTimeIntervalSinceNow(0.005));
        });
        // A run loop without sources may return immediately. Avoid a busy loop.
        std::thread::sleep(Duration::from_millis(5));
    }
    log.write(format!("Listening run finished. Underruns: {underflows}. Native controls and hearing results require your report."))?;
    if underflows > 0 {
        return Err("Listening run had underruns; do not mark hardware acceptance passed".into());
    }
    Ok(())
}

fn supported(arch: &str, major: isize, minor: isize) -> bool {
    arch == "aarch64" && (major > 12 || (major == 12 && minor >= 3))
}

struct Audio {
    renderer: Retained<AVSampleBufferAudioRenderer>,
    synchronizer: Retained<AVSampleBufferRenderSynchronizer>,
}

impl Audio {
    fn new() -> Self {
        // SAFETY: Public AVFoundation classes; used serially on this thread.
        // Attach before the first enqueue, explicitly allowing stereo sources.
        unsafe {
            let renderer = AVSampleBufferAudioRenderer::new();
            let synchronizer = AVSampleBufferRenderSynchronizer::new();
            renderer
                .setAllowedAudioSpatializationFormats(AVAudioSpatializationFormats::MonoAndStereo);
            synchronizer.addRenderer(ProtocolObject::from_ref(&*renderer));
            Self {
                renderer,
                synchronizer,
            }
        }
    }

    fn check_status(&self) -> Result<()> {
        // SAFETY: Access to the owned renderer is serialized on this thread.
        unsafe {
            if self.renderer.status() == AVQueuedSampleBufferRenderingStatus::Failed {
                return Err(format!("Apple renderer failed: {:?}", self.renderer.error()).into());
            }
        }
        Ok(())
    }
}

impl Drop for Audio {
    fn drop(&mut self) {
        // SAFETY: Both objects remain alive, and no callbacks were installed.
        // Stop and discard scheduled media on success and every error path.
        unsafe {
            self.synchronizer.setRate(0.0);
            self.renderer.flush();
        }
    }
}

fn status(code: i32, operation: &str) -> Result<()> {
    if code == 0 {
        Ok(())
    } else {
        Err(format!("{operation}: OSStatus {code}").into())
    }
}

fn format() -> Result<CFRetained<CMFormatDescription>> {
    let mut asbd = AudioStreamBasicDescription {
        mSampleRate: f64::from(SAMPLE_RATE),
        mFormatID: kAudioFormatLinearPCM,
        mFormatFlags: kAudioFormatFlagIsFloat | kAudioFormatFlagIsPacked,
        mBytesPerPacket: 8,
        mFramesPerPacket: 1,
        mBytesPerFrame: 8,
        mChannelsPerFrame: CHANNELS as u32,
        mBitsPerChannel: 32,
        mReserved: 0,
    };
    let mut raw = null();
    // SAFETY: All input/output pointers live through this call. No cookie or
    // layout is supplied; Apple treats untagged two-channel PCM as stereo.
    let code = unsafe {
        CMAudioFormatDescriptionCreate(
            None,
            NonNull::from(&mut asbd),
            0,
            null(),
            0,
            null(),
            None,
            NonNull::from(&mut raw),
        )
    };
    status(code, "CMAudioFormatDescriptionCreate")?;
    let raw = NonNull::new(raw.cast_mut()).ok_or("No audio format returned")?;
    // SAFETY: Create returned an owned Core Foundation reference (+1).
    Ok(unsafe { CFRetained::from_raw(raw) })
}

fn sample_buffer(
    format: &CMFormatDescription,
    start: u64,
    frames: u64,
) -> Result<CFRetained<CMSampleBuffer>> {
    let mut pcm = samples(start, frames);
    let bytes = pcm.len() * size_of::<f32>();
    let mut raw = null_mut();
    // SAFETY: Core Media allocates and owns the memory block. A valid output
    // pointer and nonzero size are supplied. No custom allocator is installed.
    let code = unsafe {
        CMBlockBuffer::create_with_memory_block(
            None,
            null_mut(),
            bytes,
            None,
            null(),
            0,
            bytes,
            0,
            NonNull::from(&mut raw),
        )
    };
    status(code, "CMBlockBufferCreateWithMemoryBlock")?;
    let raw = NonNull::new(raw).ok_or("No PCM block returned")?;
    // SAFETY: Create returned an owned reference (+1).
    let block = unsafe { CFRetained::from_raw(raw) };
    // SAFETY: Copy all PCM bytes into the owned block before pcm is dropped.
    // The destination covers exactly the allocation requested above.
    status(
        unsafe {
            CMBlockBuffer::replace_data_bytes(
                NonNull::new(pcm.as_mut_ptr().cast()).ok_or("Empty PCM")?,
                &block,
                0,
                bytes,
            )
        },
        "CMBlockBufferReplaceDataBytes",
    )?;
    // SAFETY: Valid frame values and positive timescale; kCMTimeInvalid is an
    // immutable framework constant, appropriate for PCM without decode timing.
    let timing = unsafe {
        CMSampleTimingInfo {
            duration: CMTime::new(1, SAMPLE_RATE as i32),
            presentationTimeStamp: CMTime::new(start as i64, SAMPLE_RATE as i32),
            decodeTimeStamp: kCMTimeInvalid,
        }
    };
    let frame_bytes = CHANNELS * size_of::<f32>();
    let mut raw = null_mut();
    // SAFETY: One timing/size entry describes all constant-size PCM frames.
    // Arrays remain alive through creation; sample buffer retains the block
    // and format. Data is ready and contains exactly frames * frame_bytes.
    let code = unsafe {
        CMSampleBuffer::create_ready(
            None,
            Some(&block),
            Some(format),
            frames as isize,
            1,
            &timing,
            1,
            &frame_bytes,
            NonNull::from(&mut raw),
        )
    };
    status(code, "CMSampleBufferCreateReady")?;
    let raw = NonNull::new(raw).ok_or("No sample buffer returned")?;
    // SAFETY: Create returned an owned reference (+1).
    Ok(unsafe { CFRetained::from_raw(raw) })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn native_support_requires_apple_silicon_and_monterey_point_three() {
        assert!(!supported("x86_64", 15, 0));
        assert!(!supported("aarch64", 11, 7));
        assert!(!supported("aarch64", 12, 2));
        assert!(supported("aarch64", 12, 3));
        assert!(supported("aarch64", 13, 0));
    }

    #[test]
    fn diagnostic_duration_is_bounded() {
        for seconds in ["0", "601", "-1", "abc"] {
            assert!(Options::parse(["--seconds".into(), seconds.into()]).is_err());
        }
        assert!(Options::parse(["--log".into()]).is_err());
        assert!(Options::parse(["--unknown".into()]).is_err());
        let options = Options::parse(["--check".into(), "--seconds".into(), "1".into()]).unwrap();
        assert!(options.check);
        assert_eq!(options.seconds, 1);
    }

    #[test]
    fn core_media_accepts_owned_stereo_pcm_with_frame_timestamps() {
        autoreleasepool(|_| {
            let format = format().unwrap();
            let first = sample_buffer(&format, 0, BLOCK_FRAMES).unwrap();
            let next = sample_buffer(&format, BLOCK_FRAMES, BLOCK_FRAMES).unwrap();
            // SAFETY: Fully initialized, owned buffers from the constructor.
            unsafe {
                assert_eq!(first.num_samples(), BLOCK_FRAMES as isize);
                assert_eq!(first.total_sample_size(), BLOCK_FRAMES as usize * 8);
                assert_eq!({ first.presentation_time_stamp().value }, 0);
                assert_eq!(
                    { next.presentation_time_stamp().value },
                    BLOCK_FRAMES as i64
                );
                assert_eq!(next.presentation_time_stamp().timescale, SAMPLE_RATE as i32);
            }
        });
    }
}
