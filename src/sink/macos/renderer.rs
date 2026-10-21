//! AVFoundation objects and notifications, confined to the audio worker.

use std::ptr::NonNull;
use std::sync::mpsc::Sender;

use block2::RcBlock;
use objc2::rc::Retained;
use objc2::runtime::ProtocolObject;
use objc2_av_foundation::{
    AVAudioSpatializationFormats, AVQueuedSampleBufferRendering,
    AVQueuedSampleBufferRenderingStatus, AVSampleBufferAudioRenderer,
    AVSampleBufferAudioRendererFlushTimeKey, AVSampleBufferRenderSynchronizer,
};
use objc2_core_foundation::CFRetained;
use objc2_core_media::{CMFormatDescription, CMTime};
use objc2_foundation::{NSNotification, NSNotificationCenter, NSObjectProtocol, NSValue};

use super::pcm::{frame_time, frames};
use super::queue::Chunk;
use super::{Command, Result, device, pcm};

pub(super) struct Renderer {
    renderer: Retained<AVSampleBufferAudioRenderer>,
    synchronizer: Retained<AVSampleBufferRenderSynchronizer>,
    format: CFRetained<CMFormatDescription>,
    pub(super) route: device::Route,
    observer: Option<Retained<ProtocolObject<dyn NSObjectProtocol>>>,
}

impl Renderer {
    pub(super) fn open(
        preferred: Option<&str>,
        events: Sender<Command>,
        generation: u64,
    ) -> Result<Self> {
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

    pub(super) fn observe(&mut self, events: Sender<Command>, generation: u64) {
        self.unobserve();
        let block = RcBlock::new(move |notification: NonNull<NSNotification>| {
            // SAFETY: NotificationCenter supplies a valid notification during
            // the callback. Only a plain frame number crosses to the worker.
            let notification = unsafe { notification.as_ref() };
            let name = notification.name().to_string();
            if !was_flushed_automatically(&name) {
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
            let _ = events.send(Command::FlushedAutomatically { generation, from });
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

    pub(super) fn enqueue(&self, chunk: &Chunk) -> Result<()> {
        let buffer = pcm::sample_buffer(&self.format, chunk.start, &chunk.pcm)?;
        // SAFETY: Buffer owns initialized stereo PCM with valid timestamps.
        unsafe {
            self.renderer.enqueueSampleBuffer(&buffer);
        }
        self.check()
    }

    pub(super) fn check(&self) -> Result<()> {
        // SAFETY: All renderer access is confined to this worker.
        unsafe {
            if self.renderer.status() == AVQueuedSampleBufferRenderingStatus::Failed {
                return Err(format!("AVFoundation: {:?}", self.renderer.error()));
            }
        }
        Ok(())
    }

    pub(super) fn frame(&self) -> Result<u64> {
        // SAFETY: Read on the synchronizer's owning worker.
        frames(unsafe { self.synchronizer.currentTime() })
    }

    pub(super) fn accepts_media(&self) -> bool {
        // SAFETY: Read only on the renderer's owning worker.
        unsafe { self.renderer.isReadyForMoreMediaData() }
    }

    #[cfg(test)]
    pub(super) fn rate(&self) -> f32 {
        // SAFETY: Read only on the synchronizer's owning worker.
        unsafe { self.synchronizer.rate() }
    }

    pub(super) fn ready(&self) -> bool {
        // SAFETY: Read only on the renderer's owning worker.
        unsafe {
            self.renderer
                .hasSufficientMediaDataForReliablePlaybackStart()
        }
    }

    pub(super) fn play_at(&self, frame: u64) {
        // SAFETY: The renderer is attached, primed, and all calls are serialized.
        unsafe {
            self.synchronizer.setRate_time(1.0, frame_time(frame));
        }
    }

    pub(super) fn stop(&self) {
        // SAFETY: The renderer is attached and all calls are serialized.
        unsafe {
            self.synchronizer.setRate(0.0);
        }
    }

    pub(super) fn reset(&self, frame: u64) {
        self.stop();
        self.flush();
        // SAFETY: Stop, flush, and reset cannot race with an enqueue.
        unsafe {
            self.synchronizer.setRate_time(0.0, frame_time(frame));
        }
    }

    pub(super) fn flush(&self) {
        // SAFETY: Flush and enqueue are serialized on the owning worker.
        unsafe { self.renderer.flush() };
    }
}

fn was_flushed_automatically(notification: &str) -> bool {
    // A configuration notification is advisory, not lost media. Our PCM
    // format stays stereo float and AVFoundation converts it for the route.
    // Flushing on this notification repeatedly renegotiates spatialization.
    notification.contains("WasFlushedAutomatically")
}

impl Drop for Renderer {
    fn drop(&mut self) {
        self.unobserve();
        self.reset(0);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn format_changes_do_not_restart_the_spatialization_session() {
        assert!(!was_flushed_automatically(
            "AVSampleBufferAudioRendererOutputConfigurationDidChangeNotification"
        ));
        assert!(was_flushed_automatically(
            "AVSampleBufferAudioRendererWasFlushedAutomaticallyNotification"
        ));
        assert!(!was_flushed_automatically("unrelated notification"));
    }

    #[test]
    fn renderer_removes_its_notification_observer() {
        if !super::super::supported() {
            return;
        }
        objc2::rc::autoreleasepool(|_| {
            let (tx, _rx) = std::sync::mpsc::channel();
            let mut audio = match Renderer::open(None, tx, 0) {
                Ok(audio) => audio,
                Err(error) if error == crate::sink::NO_DEVICE => return,
                Err(error) => panic!("native renderer initialization: {error}"),
            };
            assert!(audio.observer.is_some());
            audio.unobserve();
            assert!(audio.observer.is_none());
            audio.unobserve();
        });
    }
}
