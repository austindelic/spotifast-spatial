//! Playback commands, bounded buffering, preroll and renderer recovery.

use std::sync::atomic::Ordering;
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, PoisonError};
use std::thread;
use std::time::{Duration, Instant};

use fastframe_audio::Render;
use librespot_playback::SAMPLE_RATE;
use objc2::rc::autoreleasepool;

use super::super::{DRAIN_TIMEOUT, MixerRender, Queued};
use super::queue::{BLOCK, Chunk, LOOKAHEAD, MAX_LOOKAHEAD, Queue};
use super::renderer::Renderer;
use super::{CHANNELS, Command, Result, Shared, device};

const PREROLL_TIMEOUT: Duration = Duration::from_secs(2);
const TICK: Duration = Duration::from_millis(20);

pub(super) struct Worker {
    audio: Renderer,
    render: MixerRender,
    preferred: Option<String>,
    events: Sender<Command>,
    shared: Arc<Shared>,
    queue: Queue,
    source_queue: Option<Arc<Queued>>,
    refill_deadline: Option<Instant>,
    generation: u64,
    running: bool,
    recovered: bool,
    last_frame: u64,
    advanced: Instant,
    route_checked: Instant,
}

impl Worker {
    pub(super) fn open(
        preferred: Option<String>,
        mut render: MixerRender,
        events: Sender<Command>,
        shared: Arc<Shared>,
    ) -> Result<Self> {
        let audio = Renderer::open(preferred.as_deref(), events.clone(), 0)?;
        render.configure(SAMPLE_RATE, CHANNELS as u16);
        let mut worker = Self::new(audio, render, preferred, events, shared);
        // A stopped renderer must become ready before opening succeeds.
        // Failure reaches open_output's standard-output fallback.
        worker.prime(0)?;
        Ok(worker)
    }

    pub(super) fn device_name(&self) -> &str {
        &self.audio.route.name
    }

    pub(super) fn device_id(&self) -> &fastframe_audio::DeviceId {
        &self.audio.route.device_id
    }

    fn new(
        audio: Renderer,
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
            refill_deadline: None,
            generation: 0,
            running: false,
            recovered: false,
            last_frame: 0,
            advanced: Instant::now(),
            route_checked: Instant::now(),
        }
    }

    pub(super) fn run(&mut self, rx: Receiver<Command>) {
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
            Command::FlushedAutomatically { generation, from } if generation == self.generation => {
                let frame = from.unwrap_or_else(|| self.audio.frame().unwrap_or(self.last_frame));
                log::debug!("native audio automatic flush: generation={generation}, frame={frame}");
                if let Err(error) = self.refill_after_flush() {
                    self.recover(error);
                }
            }
            Command::FlushedAutomatically { .. } | Command::Shutdown => {}
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
        self.refill_deadline = None;
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
        self.refill_deadline = None;
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

    fn refill_after_flush(&mut self) -> Result<()> {
        self.generation = self.generation.wrapping_add(1);
        self.audio.observe(self.events.clone(), self.generation);
        // Apple's automatic flush leaves the timebase running. Clear any
        // buffers enqueued while its notification was in flight, then refill
        // at the current playhead without stopping or reprerolling. A rate
        // reset here temporarily tears down the spatialization session.
        self.audio.flush();
        if self.running {
            // A spatial-mode change can rebuild the underlying audio queue.
            // Restore the bounded startup-sized reservoir while it settles,
            // using real PCM and leaving the playback clock running.
            self.refill_deadline = Some(Instant::now() + PREROLL_TIMEOUT);
            let frame = self.audio.frame()?;
            self.queue.trim(frame);
            for chunk in &self.queue.pending {
                self.audio.enqueue(chunk)?;
            }
            self.last_frame = frame;
            self.advanced = Instant::now();
        }
        self.audio.check()
    }

    fn reopen(&mut self) -> Result<()> {
        // Use the last known playhead if the failed renderer cannot give one.
        let frame = self.audio.frame().unwrap_or(self.last_frame);
        self.audio.stop();
        self.generation = self.generation.wrapping_add(1);
        self.audio = Renderer::open(
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
                if self.audio.route.requires_reopen(&route) {
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
        let lookahead = self.lookahead(Instant::now());
        self.fill(frame, lookahead, false)?;
        Ok(())
    }

    fn lookahead(&mut self, now: Instant) -> u64 {
        if self.refill_deadline.is_some_and(|deadline| now < deadline) {
            MAX_LOOKAHEAD
        } else {
            self.refill_deadline = None;
            LOOKAHEAD
        }
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
            if !self.audio.accepts_media() {
                break;
            }
            let mut pcm = vec![0.0; frames as usize * CHANNELS];
            if !priming && let Some(queued) = &self.source_queue {
                let written = self.render.render_queued(&mut pcm, queued);
                pcm.truncate(written * CHANNELS);
                if pcm.is_empty() {
                    break;
                }
            } else {
                self.render.render(&mut pcm);
            }
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
        let started = Instant::now();
        let deadline = started + PREROLL_TIMEOUT;
        loop {
            self.audio.check()?;
            // The clock is stopped, so pacing this fill only delays startup.
            // Enqueue the bounded preroll immediately; wait only if the native
            // renderer still needs time to become ready or accept more PCM.
            self.fill(anchor, MAX_LOOKAHEAD, true)?;
            let queued = self.queue.next.saturating_sub(anchor);
            match preroll(queued, self.audio.ready(), Instant::now() >= deadline) {
                Preroll::Ready => {
                    log::debug!(
                        "native audio primed: anchor={anchor}, frames={queued}, elapsed_ms={}",
                        started.elapsed().as_millis()
                    );
                    return Ok(());
                }
                Preroll::TimedOut => {
                    return Err(format!(
                        "Native audio preroll timed out after 2 seconds (500 ms plus one frame limit): queued_frames={queued}, ready={}",
                        self.audio.ready()
                    ));
                }
                Preroll::Wait => {
                    thread::sleep(TICK);
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
        while written < out.len() / CHANNELS && queued.frames() > 0 && skipped < BLOCK {
            let before = queued.consumed.load(Ordering::Relaxed);
            let left = source.next().unwrap_or(0.0);
            let right = source.next().unwrap_or(0.0);
            if queued.consumed.load(Ordering::Relaxed) == before {
                skipped += 1;
                continue;
            }
            let gain = self.ramp.next_gain(target, SAMPLE_RATE);
            out[written * CHANNELS] = left * gain;
            out[written * CHANNELS + 1] = right * gain;
            written += 1;
        }
        written
    }
}

#[cfg(test)]
mod tests {
    use super::super::{pcm, supported};
    use super::*;
    use crate::sink::{Envelope, MixerSlot, TransitionSource, VolumeRamp};
    use std::ptr::NonNull;

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
    fn preroll_requires_pcm_and_readiness_and_has_a_deadline() {
        assert_eq!(preroll(0, true, false), Preroll::Wait);
        assert_eq!(preroll(LOOKAHEAD - 1, true, false), Preroll::Wait);
        assert_eq!(preroll(LOOKAHEAD, false, false), Preroll::Wait);
        assert_eq!(preroll(LOOKAHEAD, true, false), Preroll::Ready);
        assert_eq!(preroll(MAX_LOOKAHEAD, false, true), Preroll::TimedOut);
        assert_eq!(preroll(LOOKAHEAD, true, true), Preroll::TimedOut);
    }

    #[test]
    fn native_setup_and_stale_route_events_do_not_require_a_playing_device() {
        if !supported() {
            return;
        }
        autoreleasepool(|_| {
            let (tx, _rx) = mpsc::channel();
            let audio = match Renderer::open(None, tx.clone(), 0) {
                Ok(audio) => audio,
                Err(error) if error == crate::sink::NO_DEVICE => return,
                Err(error) => panic!("native renderer initialization: {error}"),
            };
            let render = MixerRender {
                volume: Arc::new(std::sync::atomic::AtomicU32::new(0.0_f32.to_bits())),
                ramp: VolumeRamp::default(),
                source: None,
                format: (0, 0),
                made: MixerSlot::default(),
            };
            let mut worker = Worker::new(audio, render, None, tx, Arc::new(Shared::default()));
            worker.queue.next = BLOCK;
            worker.queue.pending.push_back(Chunk {
                start: 0,
                pcm: vec![0.0; BLOCK as usize * 2],
            });
            worker.clear();
            worker.command(Command::FlushedAutomatically {
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
            worker.clear();
            worker.prime(0).unwrap();
            assert!(worker.audio.ready());
            assert!((LOOKAHEAD..=MAX_LOOKAHEAD).contains(&worker.queue.next));
            // SAFETY: The renderer belongs to this test thread.
            assert_eq!(worker.audio.rate(), 0.0);
            let (reply, _received) = mpsc::sync_channel(1);
            worker.command(Command::Resume(reply));
            assert!(worker.running);
            let deadline = Instant::now() + DRAIN_TIMEOUT;
            while worker.audio.frame().unwrap() == 0 && Instant::now() < deadline {
                thread::sleep(Duration::from_millis(10));
            }
            assert!(worker.audio.frame().unwrap() > 0);
            let before = worker.audio.frame().unwrap();
            let next = worker.queue.next;
            let generation = worker.generation;
            worker.command(Command::FlushedAutomatically {
                generation,
                from: Some(0),
            });
            assert_eq!(worker.generation, generation + 1);
            let refill_deadline = worker.refill_deadline.unwrap();
            assert_eq!(worker.lookahead(refill_deadline - TICK), MAX_LOOKAHEAD);
            assert_eq!(worker.lookahead(refill_deadline), LOOKAHEAD);
            assert!(worker.refill_deadline.is_none());
            assert!(
                worker.audio.frame().unwrap() >= before,
                "a mode switch never rewinds the clock"
            );
            assert_eq!(
                worker.queue.next, next,
                "a mode switch reuses queued PCM without preroll padding"
            );
            assert_eq!(worker.audio.rate(), 1.0);
            thread::sleep(Duration::from_millis(30));
            assert!(worker.audio.frame().unwrap() > before);
            let (reply, _received) = mpsc::sync_channel(1);
            worker.command(Command::Pause(Instant::now(), reply));
            assert!(!worker.running);
            assert!(worker.queue.pending.is_empty());
            assert!(worker.refill_deadline.is_none());
            assert_eq!(worker.audio.rate(), 0.0);
            worker.command(Command::FlushedAutomatically {
                generation: worker.generation,
                from: Some(0),
            });
            assert!(!worker.running);
            assert!(worker.queue.pending.is_empty());
            assert!(worker.refill_deadline.is_none());
            assert_eq!(worker.audio.rate(), 0.0);
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
            let audio = match Renderer::open(None, tx.clone(), 0) {
                Ok(audio) => audio,
                Err(error) if error == crate::sink::NO_DEVICE => return,
                Err(error) => panic!("native renderer initialization: {error}"),
            };
            let made = MixerSlot::default();
            let mut render = MixerRender {
                volume: Arc::new(std::sync::atomic::AtomicU32::new(1.0_f32.to_bits())),
                ramp: VolumeRamp::default(),
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
            // The stopped renderer accepts a full, bounded preroll in one
            // fill. It need not be fed at the playback clock's 20 ms cadence.
            worker.fill(anchor, MAX_LOOKAHEAD, true).unwrap();
            assert_eq!(worker.queue.next - anchor, MAX_LOOKAHEAD);
            worker.prime(anchor).unwrap();
            assert!(worker.audio.ready());
            assert!(
                worker
                    .queue
                    .pending
                    .iter()
                    .any(|chunk| chunk.pcm.iter().any(|sample| *sample != 0.0))
            );
            assert_eq!(worker.queue.next - anchor, MAX_LOOKAHEAD);
            assert_eq!(worker.queue.pending.front().unwrap().start, anchor);
            // Read back an actual owned Core Media buffer made from mixer PCM.
            let chunk = worker.queue.pending.back().unwrap();
            let buffer =
                pcm::sample_buffer(&pcm::format().unwrap(), chunk.start, &chunk.pcm).unwrap();
            let mut samples = vec![0.0_f32; chunk.pcm.len()];
            // SAFETY: Valid owned buffer and an equally sized output array.
            unsafe {
                assert_eq!(worker.audio.rate(), 0.0);
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
            let envelope = Envelope::open(SAMPLE_RATE, Duration::from_millis(1));
            sink.append(TransitionSource::new(
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
            let envelope = Envelope::open(SAMPLE_RATE, Duration::from_millis(1));
            sink.append(TransitionSource::new(
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
