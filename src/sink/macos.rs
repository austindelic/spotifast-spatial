//! Native stereo output, enabling the user's macOS AirPods spatialization
//! preference. Apple objects live on one worker; other threads send commands.
//! This avoids transferring Objective-C objects or blocking the UI thread.
//!
//! The worker handles playback transitions, the renderer owns Apple objects,
//! and the queue retains timestamped PCM for replay after an output failure.

mod device;
mod pcm;
mod queue;
mod renderer;
mod worker;

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Sender, SyncSender};
use std::sync::{Arc, Mutex, PoisonError};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use objc2::rc::autoreleasepool;
use objc2_foundation::NSProcessInfo;

use super::{MixerRender, Queued};
use worker::Worker;

const CHANNELS: usize = librespot_playback::NUM_CHANNELS as usize;
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
    FlushedAutomatically { generation: u64, from: Option<u64> },
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

    pub(super) fn open(preferred: Option<&str>, render: MixerRender) -> Result<Self> {
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
                    let worker = Worker::open(preferred, render, events, state);
                    match worker {
                        Ok(mut worker) => {
                            let _ = ready_tx.send(Ok(worker.device_name().to_owned()));
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
}
