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

use super::{DRAIN_TIMEOUT, MixerRender, Queued};

const BLOCK: u64 = SAMPLE_RATE as u64 / 50;
const LOOKAHEAD: u64 = SAMPLE_RATE as u64 / 4;
// Apple's preroll threshold is strictly greater than 500 ms. The approved
// one-frame margin adds 22.7 microseconds at 44.1 kHz.
const MAX_LOOKAHEAD: u64 = SAMPLE_RATE as u64 / 2 + 1;
const PREROLL_TIMEOUT: Duration = Duration::from_secs(2);
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
    Feed(Arc<Queued>),
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
        self.request_until(Instant::now() + COMMAND_TIMEOUT, command);
    }

    fn request_until(&self, deadline: Instant, command: impl FnOnce(SyncSender<()>) -> Command) {
        let (tx, rx) = mpsc::sync_channel(1);
        if self.tx.send(command(tx)).is_err()
            || rx
                .recv_timeout(deadline.saturating_duration_since(Instant::now()))
                .is_err()
        {
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
    pub(super) fn set_queue(&self, queued: Arc<Queued>) {
        let _ = self.reset.tx.send(Command::Feed(queued));
    }

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
                    let worker =
                        Native::open(preferred.as_deref(), events.clone(), 0).and_then(|audio| {
                            render.configure(SAMPLE_RATE, 2);
                            let mut worker = Worker::new(audio, render, preferred, events, state);
                            // Check actual preroll readiness while stopped. A
                            // timeout reaches open_output's standard fallback.
                            worker.prime(0)?;
                            Ok(worker)
                        });
                    match worker {
                        Ok(mut worker) => {
                            let _ = ready_tx.send(Ok(worker.audio.route.name.clone()));
                            worker.run(rx);
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
        self.reset
            .request_until(deadline, |reply| Command::Pause(deadline, reply));
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

    fn frames_to_fill(&self, frame: u64, lookahead: u64) -> u64 {
        frame
            .saturating_add(lookahead.min(MAX_LOOKAHEAD))
            .saturating_sub(self.next)
            .min(BLOCK)
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
    source_queue: Option<Arc<Queued>>,
    generation: u64,
    running: bool,
    recovered: bool,
    last_frame: u64,
    advanced: Instant,
    route_checked: Instant,
    diagnostic_at: Instant,
    diagnostic_count: u8,
    peak: f32,
    flushes: u64,
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
            source_queue: None,
            generation: 0,
            running: false,
            recovered: false,
            last_frame: 0,
            advanced: Instant::now(),
            route_checked: Instant::now(),
            diagnostic_at: Instant::now(),
            diagnostic_count: 0,
            peak: 0.0,
            flushes: 0,
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
            Command::Feed(queued) => self.source_queue = Some(queued),
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
                let result = result.and_then(|()| self.start());
                match result {
                    Ok(()) => {
                        *self
                            .shared
                            .error
                            .lock()
                            .unwrap_or_else(PoisonError::into_inner) = None;
                        self.running = true;
                        self.recovered = false;
                    }
                    Err(error) => self.shared.fail(error),
                }
                let _ = reply.send(());
            }
            Command::Pause(deadline, reply) => {
                log::debug!("native audio pause: generation={}", self.generation);
                self.drain(deadline);
                self.running = false;
                self.clear();
                let _ = reply.send(());
            }
            Command::Flush(drain, reply) => {
                log::debug!(
                    "native audio explicit flush: generation={}, drain={drain}",
                    self.generation
                );
                if drain {
                    self.drain(Instant::now() + DRAIN_TIMEOUT);
                }
                self.clear();
                if self.running
                    && let Err(error) = self.start()
                {
                    self.recover(error);
                }
                let _ = reply.send(());
            }
            Command::Route { generation, from } if generation == self.generation => {
                self.flushes += 1;
                let frame = from.unwrap_or_else(|| self.audio.frame().unwrap_or(self.last_frame));
                log::debug!("native audio automatic flush: generation={generation}, frame={frame}");
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
            self.start()?;
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
                // A default-following renderer already follows this change.
                // Replacing it also destroys the system's spatialization
                // session. Only an explicit UID change needs a new renderer.
                if route_requires_reopen(&self.audio.route, &route) {
                    self.reopen()?;
                } else {
                    self.audio.route = route;
                }
            }
        }
        let frame = self.audio.frame()?;
        if frame > self.last_frame {
            self.advanced = Instant::now();
        } else if self.advanced.elapsed() > DRAIN_TIMEOUT {
            return Err("playback clock stalled".into());
        }
        self.last_frame = frame;
        self.queue.trim(frame);
        self.fill(frame, LOOKAHEAD, false)?;
        self.diagnose(frame);
        Ok(())
    }

    fn fill(&mut self, frame: u64, lookahead: u64, priming: bool) -> Result<()> {
        loop {
            let wanted = self.queue.frames_to_fill(frame, lookahead);
            // The native queue absorbs decoder scheduling jitter. Do not
            // turn a temporarily empty rodio queue into scheduled silence:
            // leave room for its next packet on the next worker tick.
            let frames = renderable_frames(
                wanted,
                self.source_queue.as_ref().map(|queue| queue.frames()),
                priming,
            );
            if frames == 0 {
                break;
            }
            // SAFETY: This worker is the renderer's only owner and caller.
            if !unsafe { self.audio.renderer.isReadyForMoreMediaData() } {
                break;
            }
            let mut pcm = vec![0.0; frames as usize * 2];
            if !priming && let Some(queued) = &self.source_queue {
                let written = self.render.render_queued(&mut pcm, queued);
                pcm.truncate(written * 2);
                if pcm.is_empty() {
                    break;
                }
            } else {
                self.render.render(&mut pcm);
            }
            self.peak = pcm
                .iter()
                .fold(self.peak, |peak, sample| peak.max(sample.abs()));
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

    fn prime(&mut self, anchor: u64) -> Result<()> {
        let deadline = Instant::now() + PREROLL_TIMEOUT;
        let mut desired = LOOKAHEAD;
        loop {
            self.audio.check()?;
            self.fill(anchor, desired, true)?;
            let queued = self.queue.next.saturating_sub(anchor);
            match preroll(queued, self.audio.ready(), Instant::now() >= deadline) {
                Preroll::Ready => {
                    log::debug!("native audio primed: anchor={anchor}, frames={queued}");
                    return Ok(());
                }
                Preroll::TimedOut => {
                    return Err(format!(
                        "Native audio preroll timed out after 2 seconds (500 ms plus one frame limit): queued_frames={queued}, peak={:.5}, ready={}",
                        self.peak,
                        self.audio.ready()
                    ));
                }
                Preroll::Wait => {
                    thread::sleep(TICK);
                    desired = (desired + BLOCK).min(MAX_LOOKAHEAD);
                }
            }
        }
    }

    fn start(&mut self) -> Result<()> {
        let anchor = self
            .queue
            .pending
            .front()
            .map_or(self.queue.next, |chunk| chunk.start);
        self.prime(anchor)?;
        // Every start, including reset/recovery, anchors to PCM already queued.
        self.audio.play_at(anchor);
        self.last_frame = anchor;
        self.advanced = Instant::now();
        Ok(())
    }

    fn diagnose(&mut self, frame: u64) {
        // Temporary silence investigation: six scalar summaries, never audio.
        if self.diagnostic_count >= 6 || self.diagnostic_at.elapsed() < Duration::from_secs(5) {
            return;
        }
        self.diagnostic_count += 1;
        self.diagnostic_at = Instant::now();
        // SAFETY: Read only on the renderer's owning worker.
        let (volume, muted) =
            unsafe { (self.audio.renderer.volume(), self.audio.renderer.isMuted()) };
        log::info!(
            "native audio diagnostic: peak={:.5}, renderer_volume={volume:.3}, muted={muted}, clock_frame={frame}, next_pts={}, queued_ms={}, flushes={}",
            self.peak,
            self.queue.next,
            self.queue.next.saturating_sub(frame) * 1000 / u64::from(SAMPLE_RATE),
            self.flushes
        );
        self.peak = 0.0;
    }
}

#[derive(Debug, PartialEq, Eq)]
enum Preroll {
    Ready,
    Wait,
    TimedOut,
}

fn preroll(queued: u64, ready: bool, expired: bool) -> Preroll {
    if expired {
        Preroll::TimedOut
    } else if queued >= LOOKAHEAD && ready {
        Preroll::Ready
    } else {
        Preroll::Wait
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
            // Explicit priming is bounded by our timeout; keep Apple's own
            // readiness protection as well instead of forcing an empty start.
            synchronizer.setDelaysRateChangeUntilHasSufficientMediaData(true);
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
            if !needs_replay(&name) {
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

    fn ready(&self) -> bool {
        // SAFETY: Read only on the renderer's owning worker.
        unsafe {
            self.renderer
                .hasSufficientMediaDataForReliablePlaybackStart()
        }
    }

    fn play_at(&self, frame: u64) {
        // SAFETY: The renderer is attached, primed, and all calls are serialized.
        unsafe {
            self.synchronizer.setRate_time(1.0, frame_time(frame));
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

fn needs_replay(notification: &str) -> bool {
    // A configuration notification is advisory, not lost media. Our PCM
    // format stays stereo float and AVFoundation converts it for the route.
    // Flushing on this notification repeatedly renegotiates spatialization.
    notification.contains("WasFlushedAutomatically")
}

fn renderable_frames(wanted: u64, available: Option<u64>, priming: bool) -> u64 {
    if priming {
        wanted
    } else {
        wanted.min(available.unwrap_or(0))
    }
}

impl MixerRender {
    /// rodio's keep-alive source can retain silence from an earlier empty
    /// queue. Only schedule frames actually pulled from TransitionSource.
    /// Musical silence still increments its consumed counter and is retained.
    fn render_queued(&mut self, out: &mut [f32], queued: &Queued) -> usize {
        let Some(source) = &mut self.source else {
            return 0;
        };
        let target = f32::from_bits(self.volume.load(Ordering::Relaxed));
        let mut written = 0;
        let mut skipped = 0;
        while written < out.len() / 2 && queued.frames() > 0 && skipped < BLOCK {
            let before = queued.consumed.load(Ordering::Relaxed);
            let left = source.next().unwrap_or(0.0);
            let right = source.next().unwrap_or(0.0);
            if queued.consumed.load(Ordering::Relaxed) == before {
                skipped += 1;
                continue;
            }
            let gain = self.ramp.next_gain(target, SAMPLE_RATE);
            out[written * 2] = left * gain;
            out[written * 2 + 1] = right * gain;
            written += 1;
        }
        written
    }
}

fn route_requires_reopen(previous: &device::Route, next: &device::Route) -> bool {
    previous.uid != next.uid
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
    fn steady_rendering_waits_for_pcm_instead_of_scheduling_silence() {
        assert_eq!(renderable_frames(BLOCK, Some(0), false), 0);
        assert_eq!(renderable_frames(BLOCK, Some(100), false), 100);
        assert_eq!(renderable_frames(BLOCK, Some(2 * BLOCK), false), BLOCK);
        assert_eq!(renderable_frames(BLOCK, None, false), 0);
        // Opening/preroll is synchronous with the decoder, so only priming
        // permits a bounded silent preamble before the first packet arrives.
        assert_eq!(renderable_frames(BLOCK, Some(0), true), BLOCK);
    }

    #[test]
    fn format_changes_do_not_restart_the_spatialization_session() {
        assert!(!needs_replay(
            "AVSampleBufferAudioRendererOutputConfigurationDidChangeNotification"
        ));
        assert!(needs_replay(
            "AVSampleBufferAudioRendererWasFlushedAutomaticallyNotification"
        ));
        assert!(!needs_replay("unrelated notification"));
    }

    #[test]
    fn default_device_changes_keep_the_renderer_but_named_device_changes_reopen() {
        use objc2_foundation::NSString;
        let speakers = device::Route {
            id: 1,
            name: "speakers".into(),
            uid: None,
        };
        let airpods = device::Route {
            id: 2,
            name: "AirPods".into(),
            uid: None,
        };
        let named = device::Route {
            id: 2,
            name: "AirPods".into(),
            uid: Some(NSString::from_str("airpods")),
        };
        assert!(!route_requires_reopen(&speakers, &airpods));
        assert!(!route_requires_reopen(&airpods, &speakers));
        assert!(route_requires_reopen(&airpods, &named));
        assert!(route_requires_reopen(&named, &speakers));
        assert!(!route_requires_reopen(&named, &named));
    }

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
            while queue.frames_to_fill(frame, LOOKAHEAD) > 0 {
                let chunk = Chunk {
                    start: queue.next,
                    pcm: vec![0.5; queue.frames_to_fill(frame, LOOKAHEAD) as usize * 2],
                };
                queue.next = chunk.end();
                queue.pending.push_back(chunk);
            }
            assert!(queue.next <= frame + LOOKAHEAD);
            assert!(queue.pending.len() <= LOOKAHEAD.div_ceil(BLOCK) as usize + 1);
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
        assert!(queue.frames_to_fill(BLOCK, LOOKAHEAD) > 0);
    }

    #[test]
    fn preroll_requires_pcm_and_readiness_and_has_a_deadline() {
        assert_eq!(preroll(0, true, false), Preroll::Wait);
        assert_eq!(preroll(LOOKAHEAD - 1, true, false), Preroll::Wait);
        assert_eq!(preroll(LOOKAHEAD, false, false), Preroll::Wait);
        assert_eq!(preroll(LOOKAHEAD, true, false), Preroll::Ready);
        assert_eq!(preroll(MAX_LOOKAHEAD, false, true), Preroll::TimedOut);
        assert_eq!(preroll(LOOKAHEAD, true, true), Preroll::TimedOut);
    }

    #[test]
    fn partial_chunks_fill_the_target_without_crossing_the_hard_limit() {
        let mut queue = Queue::default();
        while queue.frames_to_fill(0, u64::MAX) > 0 {
            queue.next += queue.frames_to_fill(0, u64::MAX);
        }
        assert_eq!(queue.next, MAX_LOOKAHEAD);
        assert_eq!(queue.frames_to_fill(0, LOOKAHEAD), 0);
        queue.next = LOOKAHEAD - 1;
        assert_eq!(queue.frames_to_fill(0, LOOKAHEAD), 1);
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
            worker.clear();
            worker.prime(0).unwrap();
            assert!(worker.audio.ready());
            assert!((LOOKAHEAD..=MAX_LOOKAHEAD).contains(&worker.queue.next));
            // SAFETY: The renderer belongs to this test thread.
            assert_eq!(unsafe { worker.audio.synchronizer.rate() }, 0.0);
            let (reply, _received) = mpsc::sync_channel(1);
            worker.command(Command::Resume(reply));
            assert!(worker.running);
            let deadline = Instant::now() + DRAIN_TIMEOUT;
            while worker.audio.frame().unwrap() == 0 && Instant::now() < deadline {
                thread::sleep(Duration::from_millis(10));
            }
            assert!(worker.audio.frame().unwrap() > 0);
            let (reply, _received) = mpsc::sync_channel(1);
            worker.command(Command::Pause(Instant::now(), reply));
            assert!(!worker.running);
            assert!(worker.queue.pending.is_empty());
            assert_eq!(unsafe { worker.audio.synchronizer.rate() }, 0.0);
            // The source is None, so this test renders only silence.
        });
    }

    #[test]
    fn nonzero_mixer_pcm_is_owned_and_primed_while_clock_is_stopped() {
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
            let made = super::super::MixerSlot::default();
            let mut render = MixerRender {
                volume: Arc::new(std::sync::atomic::AtomicU32::new(1.0_f32.to_bits())),
                ramp: super::super::VolumeRamp::default(),
                source: None,
                format: (0, 0),
                made: Arc::clone(&made),
            };
            render.configure(SAMPLE_RATE, 2);
            let (mixer, _) = made.lock().unwrap().take().unwrap();
            let sink = rodio::Sink::connect_new(&mixer);
            sink.append(rodio::buffer::SamplesBuffer::new(
                2,
                SAMPLE_RATE,
                vec![0.25_f32; MAX_LOOKAHEAD as usize * 2],
            ));
            let mut worker = Worker::new(audio, render, None, tx, Arc::new(Shared::default()));
            let anchor = 17 * u64::from(SAMPLE_RATE);
            worker.audio.reset(anchor);
            worker.queue.next = anchor;
            worker.prime(anchor).unwrap();
            assert!(worker.audio.ready());
            assert!(worker.peak > 0.0);
            assert!((LOOKAHEAD..=MAX_LOOKAHEAD).contains(&(worker.queue.next - anchor)));
            assert_eq!(worker.queue.pending.front().unwrap().start, anchor);
            // Read back an actual owned Core Media buffer made from mixer PCM.
            let chunk = worker.queue.pending.back().unwrap();
            let buffer = pcm::sample_buffer(&worker.audio.format, chunk.start, &chunk.pcm).unwrap();
            let mut samples = vec![0.0_f32; chunk.pcm.len()];
            // SAFETY: Valid owned buffer and an equally sized output array.
            unsafe {
                assert_eq!(worker.audio.synchronizer.rate(), 0.0);
                assert_eq!(
                    buffer.data_buffer().unwrap().copy_data_bytes(
                        0,
                        size_of_val(samples.as_slice()),
                        NonNull::new(samples.as_mut_ptr().cast()).unwrap()
                    ),
                    0
                );
            }
            assert_eq!(samples, chunk.pcm);
            assert!(samples.iter().any(|sample| *sample != 0.0));
            assert_eq!(worker.audio.frame().unwrap(), anchor);
            worker.clear();
            assert!(worker.queue.pending.is_empty());
            let queued = Queued::new();
            queued.appended.store(100, Ordering::Relaxed);
            let envelope = super::super::Envelope::open(SAMPLE_RATE, Duration::from_millis(1));
            sink.append(super::super::TransitionSource::new(
                rodio::buffer::SamplesBuffer::new(2, SAMPLE_RATE, vec![0.25; 200]),
                Arc::clone(&envelope),
                envelope,
                Arc::clone(&queued),
                100,
            ));
            worker.command(Command::Feed(Arc::clone(&queued)));
            worker.fill(0, LOOKAHEAD, false).unwrap();
            assert_eq!(queued.frames(), 0);
            assert_eq!(worker.queue.next, 100, "do not pad a short decoder packet");
            assert_eq!(worker.queue.pending.len(), 1);
            assert!(
                worker.queue.pending[0]
                    .pcm
                    .iter()
                    .all(|sample| *sample == 0.25)
            );
            worker.fill(0, LOOKAHEAD, false).unwrap();
            assert_eq!(worker.queue.next, 100, "wait without injecting silence");
            queued.appended.fetch_add(100, Ordering::Relaxed);
            let envelope = super::super::Envelope::open(SAMPLE_RATE, Duration::from_millis(1));
            sink.append(super::super::TransitionSource::new(
                rodio::buffer::SamplesBuffer::new(2, SAMPLE_RATE, vec![0.0; 200]),
                Arc::clone(&envelope),
                envelope,
                Arc::clone(&queued),
                100,
            ));
            worker.fill(0, LOOKAHEAD, false).unwrap();
            assert_eq!(
                worker.queue.next, 200,
                "real silent samples retain their duration"
            );
            assert_eq!(
                worker.queue.pending[1].start, 100,
                "the next packet stays contiguous"
            );
            assert!(
                worker.queue.pending[1]
                    .pcm
                    .iter()
                    .all(|sample| *sample == 0.0)
            );
            // No playback starts: this verifies nonzero data, not audibility.
        });
    }
}
