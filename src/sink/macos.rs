//! Native stereo output, enabling the user's macOS AirPods spatialization
//! preference. Apple objects live on one worker; other threads send commands.
//! This avoids transferring Objective-C objects or blocking the UI thread.

mod device;
mod pcm;

use std::collections::VecDeque;
use std::ptr::NonNull;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, Sender, SyncSender};
use std::sync::{Arc, Mutex, PoisonError};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use block2::RcBlock;
use fastframe_audio::Render;
use librespot_playback::SAMPLE_RATE;
use objc2::rc::{Retained, autoreleasepool};
use objc2::runtime::ProtocolObject;
use objc2_av_foundation::{
    AVAudioSpatializationFormats, AVQueuedSampleBufferRendering,
    AVQueuedSampleBufferRenderingStatus, AVSampleBufferAudioRenderer,
    AVSampleBufferAudioRendererFlushTimeKey, AVSampleBufferRenderSynchronizer,
};
use objc2_core_foundation::CFRetained;
use objc2_core_media::{CMFormatDescription, CMTime};
use objc2_foundation::{
    NSNotification, NSNotificationCenter, NSObjectProtocol, NSProcessInfo, NSValue,
};

use super::{DRAIN_TIMEOUT, MixerRender};

const BLOCK: u64 = SAMPLE_RATE as u64 / 50;
const LOOKAHEAD: u64 = BLOCK * 2;
const TICK: Duration = Duration::from_millis(20);
const COMMAND_TIMEOUT: Duration = Duration::from_secs(3);
type Result<T> = std::result::Result<T, String>;

pub(super) fn supported() -> bool {
    let version = NSProcessInfo::processInfo().operatingSystemVersion();
    eligible(
        cfg!(target_arch = "aarch64"),
        version.majorVersion,
        version.minorVersion,
    )
}

fn eligible(apple_silicon: bool, major: isize, minor: isize) -> bool {
    apple_silicon && (major > 12 || (major == 12 && minor >= 3))
}

#[derive(Default)]
struct Shared {
    active: AtomicBool,
    error: Mutex<Option<String>>,
}

impl Shared {
    fn fail(&self, error: String) {
        log::error!("native audio: {error}");
        *self.error.lock().unwrap_or_else(PoisonError::into_inner) = Some(error);
    }
}

enum Command {
    Resume(SyncSender<()>),
    Pause(Instant, SyncSender<()>),
    Flush(bool, SyncSender<()>),
    Route { generation: u64, from: Option<u64> },
    Shutdown,
}

/// Also held by AudioControl so a confirmed seek can discard native buffers
/// immediately, even while the decoder is waiting for its next packet.
#[derive(Clone)]
pub(super) struct Reset {
    tx: Sender<Command>,
    shared: Arc<Shared>,
}

impl Reset {
    fn request(&self, command: impl FnOnce(SyncSender<()>) -> Command) {
        let (tx, rx) = mpsc::sync_channel(1);
        if self.tx.send(command(tx)).is_err() || rx.recv_timeout(COMMAND_TIMEOUT).is_err() {
            self.shared
                .fail("The native audio worker stopped responding".into());
        }
    }

    pub(super) fn flush(&self, drain: bool) {
        self.request(|reply| Command::Flush(drain, reply));
    }
}

pub(super) struct Output {
    reset: Reset,
    worker: Option<JoinHandle<()>>,
    name: String,
}

impl Output {
    pub(super) fn open(preferred: Option<&str>, mut render: MixerRender) -> Result<Self> {
        let preferred = preferred.map(str::to_owned);
        let shared = Arc::new(Shared::default());
        let state = Arc::clone(&shared);
        let (tx, rx) = mpsc::channel();
        let events = tx.clone();
        let (ready_tx, ready_rx) = mpsc::sync_channel(1);
        let worker = thread::Builder::new()
            .name("apple-audio".into())
            .spawn(move || {
                autoreleasepool(|_| {
                    let audio = Native::open(preferred.as_deref(), events.clone(), 0);
                    match audio {
                        Ok(audio) => {
                            render.configure(SAMPLE_RATE, 2);
                            let _ = ready_tx.send(Ok(audio.route.name.clone()));
                            Worker::new(audio, render, preferred, events, state).run(rx);
                        }
                        Err(error) => {
                            let _ = ready_tx.send(Err(error));
                        }
                    }
                });
            })
            .map_err(|error| error.to_string())?;
        let name = match ready_rx.recv_timeout(COMMAND_TIMEOUT) {
            Ok(Ok(name)) => name,
            result => {
                let _ = tx.send(Command::Shutdown);
                return Err(match result {
                    Ok(Err(error)) => error,
                    _ => "Opening the native audio output timed out".into(),
                });
            }
        };
        log::info!("audio output: AVFoundation stereo spatialization permitted (macOS controls)");
        Ok(Self {
            reset: Reset { tx, shared },
            worker: Some(worker),
            name,
        })
    }

    pub(super) fn reset_handle(&self) -> Reset {
        self.reset.clone()
    }
    pub(super) fn device_name(&self) -> &str {
        &self.name
    }
    pub(super) fn failed(&self) -> bool {
        self.maintain().is_err()
    }

    pub(super) fn maintain(&self) -> Result<()> {
        match self
            .reset
            .shared
            .error
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
        {
            Some(error) => Err(error),
            None => Ok(()),
        }
    }

    pub(super) fn resume(&mut self) {
        if !self.reset.shared.active.swap(true, Ordering::SeqCst) {
            self.reset.request(Command::Resume);
        }
    }

    pub(super) fn pause(&mut self, deadline: Instant) {
        self.reset.shared.active.store(false, Ordering::SeqCst);
        self.reset.request(|reply| Command::Pause(deadline, reply));
    }

    #[cfg(test)]
    pub(super) fn is_paused(&self) -> bool {
        !self.reset.shared.active.load(Ordering::SeqCst)
    }
}

impl Drop for Output {
    fn drop(&mut self) {
        let _ = self.reset.tx.send(Command::Shutdown);
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

struct Chunk {
    start: u64,
    pcm: Vec<f32>,
}

impl Chunk {
    fn end(&self) -> u64 {
        self.start + (self.pcm.len() / 2) as u64
    }
}

#[derive(Default)]
struct Queue {
    next: u64,
    pending: VecDeque<Chunk>,
}

impl Queue {
    fn played(&mut self, frame: u64) {
        while self
            .pending
            .front()
            .is_some_and(|chunk| chunk.end() <= frame)
        {
            self.pending.pop_front();
        }
        // After an underrun, preserve the next unconsumed mixer sample and
        // schedule it now instead of feeding expired presentation timestamps.
        self.next = self.next.max(frame);
    }

    fn has_room(&self, frame: u64) -> bool {
        self.next + BLOCK <= frame.saturating_add(LOOKAHEAD)
    }

    fn trim(&mut self, frame: u64) {
        self.played(frame);
        if let Some(first) = self.pending.front_mut()
            && first.start < frame
        {
            first.pcm.drain(..((frame - first.start) * 2) as usize);
            first.start = frame;
        }
    }
}

struct Worker {
    audio: Native,
    render: MixerRender,
    preferred: Option<String>,
    events: Sender<Command>,
    shared: Arc<Shared>,
    queue: Queue,
    generation: u64,
    running: bool,
    recovered: bool,
    last_frame: u64,
    advanced: Instant,
    route_checked: Instant,
}

impl Worker {
    fn new(
        audio: Native,
        render: MixerRender,
        preferred: Option<String>,
        events: Sender<Command>,
        shared: Arc<Shared>,
    ) -> Self {
        Self {
            audio,
            render,
            preferred,
            events,
            shared,
            queue: Queue::default(),
            generation: 0,
            running: false,
            recovered: false,
            last_frame: 0,
            advanced: Instant::now(),
            route_checked: Instant::now(),
        }
    }

    fn run(&mut self, rx: Receiver<Command>) {
        loop {
            let command = if self.running {
                match rx.recv_timeout(TICK) {
                    Ok(command) => Some(command),
                    Err(mpsc::RecvTimeoutError::Timeout) => None,
                    Err(mpsc::RecvTimeoutError::Disconnected) => break,
                }
            } else {
                match rx.recv() {
                    Ok(command) => Some(command),
                    Err(_) => break,
                }
            };
            if matches!(command, Some(Command::Shutdown)) {
                break;
            }
            autoreleasepool(|_| {
                if let Some(command) = command {
                    self.command(command);
                }
                if self.running
                    && let Err(error) = self.tick()
                {
                    self.recover(error);
                }
            });
        }
    }

    fn command(&mut self, command: Command) {
        match command {
            Command::Resume(reply) => {
                let result = if self
                    .shared
                    .error
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .is_some()
                {
                    self.reopen()
                } else {
                    Ok(())
                };
                match result {
                    Ok(()) => {
                        *self
                            .shared
                            .error
                            .lock()
                            .unwrap_or_else(PoisonError::into_inner) = None;
                        self.running = true;
                        self.recovered = false;
                        self.advanced = Instant::now();
                        self.audio.play();
                    }
                    Err(error) => self.shared.fail(error),
                }
                let _ = reply.send(());
            }
            Command::Pause(deadline, reply) => {
                self.drain(deadline);
                self.running = false;
                self.clear();
                let _ = reply.send(());
            }
            Command::Flush(drain, reply) => {
                if drain {
                    self.drain(Instant::now() + Duration::from_millis(100));
                }
                self.clear();
                if self.running {
                    self.audio.play();
                }
                let _ = reply.send(());
            }
            Command::Route { generation, from } if generation == self.generation => {
                let frame = from.unwrap_or(self.last_frame);
                if let Err(error) = self.replay(frame) {
                    self.recover(error);
                }
            }
            Command::Route { .. } | Command::Shutdown => {}
        }
    }

    fn recover(&mut self, error: String) {
        if self.recovered || self.reopen().is_err() {
            self.running = false;
            self.audio.stop();
            self.shared
                .fail(format!("The native audio output stopped working: {error}"));
        } else {
            self.recovered = true;
            log::warn!("native audio output reopened after failure: {error}");
        }
    }

    fn clear(&mut self) {
        self.generation = self.generation.wrapping_add(1);
        self.audio.observe(self.events.clone(), self.generation);
        self.audio.reset(0);
        self.queue = Queue::default();
        self.last_frame = 0;
        self.advanced = Instant::now();
    }

    fn drain(&mut self, deadline: Instant) {
        // No more mixer reads here. Drain the extra native lookahead as well
        // as rodio before pausing, so the last part of the fade is audible.
        while self.running && Instant::now() < deadline {
            match self.audio.frame() {
                Ok(frame) if frame < self.queue.next => thread::sleep(Duration::from_millis(2)),
                _ => break,
            }
        }
    }

    fn replay(&mut self, frame: u64) -> Result<()> {
        self.queue.trim(frame);
        self.generation = self.generation.wrapping_add(1);
        self.audio.observe(self.events.clone(), self.generation);
        self.audio.reset(frame);
        for chunk in &self.queue.pending {
            self.audio.enqueue(chunk)?;
        }
        self.last_frame = frame;
        self.advanced = Instant::now();
        if self.running {
            self.audio.play();
        }
        Ok(())
    }

    fn reopen(&mut self) -> Result<()> {
        // Use the last known playhead if the failed renderer cannot give one.
        let frame = self.audio.frame().unwrap_or(self.last_frame);
        self.audio.stop();
        self.generation = self.generation.wrapping_add(1);
        self.audio = Native::open(
            self.preferred.as_deref(),
            self.events.clone(),
            self.generation,
        )?;
        self.replay(frame)
    }

    fn tick(&mut self) -> Result<()> {
        self.audio.check()?;
        if self.route_checked.elapsed() >= Duration::from_secs(2) {
            self.route_checked = Instant::now();
            let route = device::resolve(self.preferred.as_deref())?;
            if route.id != self.audio.route.id
                || route.uid.is_some() != self.audio.route.uid.is_some()
            {
                log::info!("native audio output changed: {}", route.name);
                self.reopen()?;
            }
        }
        let frame = self.audio.frame()?;
        if frame > self.last_frame {
            self.advanced = Instant::now();
        } else if self.advanced.elapsed() > DRAIN_TIMEOUT {
            return Err("playback clock stalled".into());
        }
        self.last_frame = frame;
        self.queue.played(frame);
        while self.queue.has_room(frame) {
            // SAFETY: This worker is the renderer's only owner and caller.
            if !unsafe { self.audio.renderer.isReadyForMoreMediaData() } {
                break;
            }
            let mut pcm = vec![0.0; BLOCK as usize * 2];
            self.render.render(&mut pcm);
            let chunk = Chunk {
                start: self.queue.next,
                pcm,
            };
            // Retain before enqueueing, so an enqueue failure can replay it.
            self.queue.next = chunk.end();
            self.queue.pending.push_back(chunk);
            self.audio
                .enqueue(self.queue.pending.back().expect("just appended"))?;
        }
        Ok(())
    }
}

struct Native {
    renderer: Retained<AVSampleBufferAudioRenderer>,
    synchronizer: Retained<AVSampleBufferRenderSynchronizer>,
    format: CFRetained<CMFormatDescription>,
    route: device::Route,
    observer: Option<Retained<ProtocolObject<dyn NSObjectProtocol>>>,
}

impl Native {
    fn open(preferred: Option<&str>, events: Sender<Command>, generation: u64) -> Result<Self> {
        // AVFoundation can report invalid device configuration by exception.
        // Return it through the ordinary initialization fallback. No partially
        // initialized renderer is reused after unwinding this construction.
        objc2::exception::catch(std::panic::AssertUnwindSafe(|| {
            Self::build(preferred, events, generation)
        }))
        .map_err(|exception| format!("AVFoundation initialization: {exception:?}"))?
    }

    fn build(preferred: Option<&str>, events: Sender<Command>, generation: u64) -> Result<Self> {
        let route = device::resolve(preferred)?;
        if let Some(name) = preferred.filter(|name| !name.trim().is_empty())
            && route.uid.is_none()
        {
            log::warn!(
                "audio output {name:?} is unavailable; using default: {}",
                route.name
            );
        }
        let format = pcm::format()?;
        // SAFETY: Called only after supported() and on the owning worker.
        // Attach to the synchronizer before sending any PCM buffers.
        let (renderer, synchronizer) = unsafe {
            let renderer = AVSampleBufferAudioRenderer::new();
            renderer
                .setAllowedAudioSpatializationFormats(AVAudioSpatializationFormats::MonoAndStereo);
            // A fresh renderer follows the default. Despite the nullable
            // getter, macOS throws if the setter is explicitly given nil.
            if let Some(uid) = route.uid.as_deref() {
                renderer.setAudioOutputDeviceUniqueID(Some(uid));
            }
            let synchronizer = AVSampleBufferRenderSynchronizer::new();
            // We bound our own lookahead. Waiting for AVFoundation's larger
            // preroll threshold could otherwise leave this small queue stalled.
            synchronizer.setDelaysRateChangeUntilHasSufficientMediaData(false);
            synchronizer.addRenderer(ProtocolObject::from_ref(&*renderer));
            (renderer, synchronizer)
        };
        let mut native = Self {
            renderer,
            synchronizer,
            format,
            route,
            observer: None,
        };
        // Validate PCM setup without starting audio or consuming the mixer.
        native.enqueue(&Chunk {
            start: 0,
            pcm: vec![0.0; BLOCK as usize * 2],
        })?;
        native.check()?;
        native.reset(0);
        native.observe(events, generation);
        Ok(native)
    }

    fn observe(&mut self, events: Sender<Command>, generation: u64) {
        self.unobserve();
        let block = RcBlock::new(move |notification: NonNull<NSNotification>| {
            // SAFETY: NotificationCenter supplies a valid notification during
            // the callback. Only a plain frame number crosses to the worker.
            let notification = unsafe { notification.as_ref() };
            let name = notification.name().to_string();
            if !name.contains("WasFlushedAutomatically")
                && !name.contains("OutputConfigurationDidChange")
            {
                return;
            }
            let from = notification.userInfo().and_then(|info| {
                // SAFETY: This public key exists since macOS 10.13; its value
                // is documented as an NSValue wrapping CMTime.
                let value =
                    info.objectForKey(unsafe { AVSampleBufferAudioRendererFlushTimeKey })?;
                let value = value.downcast_ref::<NSValue>()?;
                let mut time = frame_time(0);
                // SAFETY: The destination has the documented CMTime size.
                unsafe {
                    value.getValue_size(NonNull::from(&mut time).cast(), size_of::<CMTime>());
                }
                frames(time).ok()
            });
            let _ = events.send(Command::Route { generation, from });
        });
        // Observe this renderer only. Filtering names avoids linking the
        // macOS-12 configuration notification symbol on macOS 11 builds.
        // SAFETY: The block captures only a Send channel and an integer. It
        // never accesses the renderer and can run on any notification thread.
        self.observer = Some(unsafe {
            NSNotificationCenter::defaultCenter().addObserverForName_object_queue_usingBlock(
                None,
                Some(&self.renderer),
                None,
                &block,
            )
        });
    }

    fn unobserve(&mut self) {
        if let Some(observer) = self.observer.take() {
            // SAFETY: This token belongs to this center; any in-flight block
            // owns only its channel and carries the obsolete generation.
            unsafe {
                NSNotificationCenter::defaultCenter().removeObserver(AsRef::<
                    objc2::runtime::AnyObject,
                >::as_ref(
                    &*observer
                ));
            }
        }
    }

    fn enqueue(&self, chunk: &Chunk) -> Result<()> {
        let buffer = pcm::sample_buffer(&self.format, chunk.start, &chunk.pcm)?;
        // SAFETY: Buffer owns initialized stereo PCM with valid timestamps.
        unsafe {
            self.renderer.enqueueSampleBuffer(&buffer);
        }
        self.check()
    }

    fn check(&self) -> Result<()> {
        // SAFETY: All renderer access is confined to this worker.
        unsafe {
            if self.renderer.status() == AVQueuedSampleBufferRenderingStatus::Failed {
                return Err(format!("AVFoundation: {:?}", self.renderer.error()));
            }
        }
        Ok(())
    }

    fn frame(&self) -> Result<u64> {
        // SAFETY: Read on the synchronizer's owning worker.
        frames(unsafe { self.synchronizer.currentTime() })
    }

    fn play(&self) {
        // SAFETY: The renderer is attached and all calls are serialized.
        unsafe {
            self.synchronizer.setRate(1.0);
        }
    }

    fn stop(&self) {
        // SAFETY: The renderer is attached and all calls are serialized.
        unsafe {
            self.synchronizer.setRate(0.0);
        }
    }

    fn reset(&self, frame: u64) {
        self.stop();
        // SAFETY: Stop, flush, and reset cannot race with an enqueue.
        unsafe {
            self.renderer.flush();
            self.synchronizer.setRate_time(0.0, frame_time(frame));
        }
    }
}

impl Drop for Native {
    fn drop(&mut self) {
        self.unobserve();
        self.reset(0);
    }
}

fn frame_time(frame: u64) -> CMTime {
    // SAFETY: Fixed positive timescale; frame counts fit within i64 in practice.
    unsafe { CMTime::new(frame as i64, SAMPLE_RATE as i32) }
}

fn frames(time: CMTime) -> Result<u64> {
    // SAFETY: Value returned by Core Media or a documented flush notification.
    let seconds = unsafe { time.seconds() };
    if !seconds.is_finite() || seconds < 0.0 {
        return Err("Invalid native audio clock".into());
    }
    Ok((seconds * f64::from(SAMPLE_RATE)).floor() as u64)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn eligibility_preserves_intel_and_old_macos() {
        assert!(!eligible(false, 26, 0));
        assert!(!eligible(true, 11, 7));
        assert!(!eligible(true, 12, 2));
        assert!(eligible(true, 12, 3));
        assert!(eligible(true, 13, 0));
    }

    #[test]
    fn buffering_is_bounded_even_after_a_late_wakeup() {
        let mut queue = Queue::default();
        for frame in [0, BLOCK, 17 * BLOCK, 18 * BLOCK] {
            queue.played(frame);
            while queue.has_room(frame) {
                let chunk = Chunk {
                    start: queue.next,
                    pcm: vec![0.5; BLOCK as usize * 2],
                };
                queue.next = chunk.end();
                queue.pending.push_back(chunk);
            }
            assert!(queue.next <= frame + LOOKAHEAD);
            assert!(queue.pending.len() <= 2);
        }
    }

    #[test]
    fn recovery_keeps_only_unplayed_samples_without_advancing_the_mixer() {
        let mut queue = Queue {
            next: 4,
            pending: VecDeque::from([Chunk {
                start: 0,
                pcm: vec![1., 1., 2., 2., 3., 3., 4., 4.],
            }]),
        };
        queue.trim(2);
        assert_eq!(queue.pending[0].start, 2);
        assert_eq!(queue.pending[0].pcm, [3., 3., 4., 4.]);
        assert_eq!(queue.next, 4);
        queue.trim(4);
        assert!(queue.pending.is_empty());
    }

    #[test]
    fn natural_track_boundaries_keep_the_next_frame_timestamp() {
        let mut queue = Queue {
            next: BLOCK,
            pending: VecDeque::from([Chunk {
                start: 0,
                pcm: vec![0.0; BLOCK as usize * 2],
            }]),
        };
        queue.played(BLOCK);
        assert!(queue.pending.is_empty());
        assert_eq!(queue.next, BLOCK, "an empty queue is not an explicit flush");
        assert!(queue.has_room(BLOCK));
    }

    #[test]
    fn owned_pcm_has_stereo_frame_sizes_and_contiguous_timestamps() {
        let format = pcm::format().unwrap();
        assert!(pcm::sample_buffer(&format, 0, &[]).is_err());
        assert!(pcm::sample_buffer(&format, 0, &[1.0]).is_err());
        let buffer = pcm::sample_buffer(&format, BLOCK, &vec![0.0; BLOCK as usize * 2]).unwrap();
        // SAFETY: Owned buffer created above; getters do not mutate it.
        unsafe {
            assert_eq!(buffer.num_samples(), BLOCK as isize);
            assert_eq!(buffer.total_sample_size(), BLOCK as usize * 8);
            assert_eq!({ buffer.presentation_time_stamp().value }, BLOCK as i64);
            assert_eq!(
                buffer.presentation_time_stamp().timescale,
                SAMPLE_RATE as i32
            );
        }
    }

    #[test]
    fn enqueued_pcm_does_not_borrow_reused_mixer_memory() {
        let format = pcm::format().unwrap();
        let mut scratch = vec![0.25_f32, -0.5, 0.75, -1.0];
        let buffer = pcm::sample_buffer(&format, 0, &scratch).unwrap();
        scratch.fill(0.0);
        let mut copied = [0.0_f32; 4];
        // SAFETY: The sample buffer owns 16 initialized bytes. The destination
        // is exactly that size, and its memory remains alive through the copy.
        unsafe {
            let block = buffer.data_buffer().unwrap();
            assert_eq!(
                block.copy_data_bytes(
                    0,
                    size_of_val(&copied),
                    NonNull::new(copied.as_mut_ptr().cast()).unwrap()
                ),
                0
            );
        }
        assert_eq!(copied, [0.25, -0.5, 0.75, -1.0]);
    }

    #[test]
    fn native_setup_and_stale_route_events_do_not_require_a_playing_device() {
        if !supported() {
            return;
        }
        autoreleasepool(|_| {
            let (tx, _rx) = mpsc::channel();
            let audio = match Native::open(None, tx.clone(), 0) {
                Ok(audio) => audio,
                Err(error) if error == crate::sink::NO_DEVICE => return,
                Err(error) => panic!("native renderer initialization: {error}"),
            };
            let render = MixerRender {
                volume: Arc::new(std::sync::atomic::AtomicU32::new(0.0_f32.to_bits())),
                ramp: super::super::VolumeRamp::default(),
                source: None,
                format: (0, 0),
                made: super::super::MixerSlot::default(),
            };
            let mut worker = Worker::new(audio, render, None, tx, Arc::new(Shared::default()));
            worker.queue.next = BLOCK;
            worker.queue.pending.push_back(Chunk {
                start: 0,
                pcm: vec![0.0; BLOCK as usize * 2],
            });
            worker.clear();
            worker.command(Command::Route {
                generation: 0,
                from: Some(BLOCK),
            });
            assert_eq!(
                worker.queue.next, 0,
                "a stale route event cannot restore the old position"
            );
            assert!(worker.queue.pending.is_empty());
            assert!(!worker.running);
            // A stalled clock cannot extend a stop beyond its caller's deadline.
            worker.running = true;
            worker.queue.next = BLOCK;
            let started = Instant::now();
            worker.drain(started + Duration::from_millis(10));
            assert!(started.elapsed() < Duration::from_millis(250));
            worker.running = false;
            worker.audio.unobserve();
            assert!(worker.audio.observer.is_none());
            worker.audio.unobserve();
            // Rate stays zero for this entire test; it never plays a sound.
        });
    }
}
