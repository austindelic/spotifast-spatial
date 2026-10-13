//! Owned Core Media PCM. No sample buffer borrows the mixer's scratch memory.
use librespot_playback::SAMPLE_RATE;
use objc2_core_audio_types::{
    AudioStreamBasicDescription, kAudioFormatFlagIsFloat, kAudioFormatFlagIsPacked,
    kAudioFormatLinearPCM,
};
use objc2_core_foundation::CFRetained;
use objc2_core_media::{
    CMAudioFormatDescriptionCreate, CMBlockBuffer, CMFormatDescription, CMSampleBuffer,
    CMSampleTimingInfo, CMTime, kCMTimeInvalid,
};
use std::ptr::{NonNull, null, null_mut};
const CHANNELS: usize = 2;
type Result<T> = std::result::Result<T, String>;

fn status(code: i32, operation: &str) -> Result<()> {
    if code == 0 {
        Ok(())
    } else {
        Err(format!("{operation}: OSStatus {code}"))
    }
}

pub(super) fn format() -> Result<CFRetained<CMFormatDescription>> {
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

pub(super) fn sample_buffer(
    format: &CMFormatDescription,
    start: u64,
    pcm: &[f32],
) -> Result<CFRetained<CMSampleBuffer>> {
    if pcm.is_empty() || !pcm.len().is_multiple_of(CHANNELS) {
        return Err("PCM must contain complete stereo frames".into());
    }
    let frames = pcm.len() / CHANNELS;
    let bytes = std::mem::size_of_val(pcm);
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
                NonNull::new(pcm.as_ptr().cast_mut().cast()).ok_or("Empty PCM")?,
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
