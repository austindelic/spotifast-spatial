//! Platform output transport. Mixing, processing and playback controls remain shared.

use std::sync::Arc;
use std::time::Instant;

use fastframe_audio::{Maintained, Reason};

#[cfg(target_os = "macos")]
use super::macos;
use super::{MixerRender, OpenError, Queued, output_options};

/// Only the device transport differs. The mixer, processing and controls stay
/// shared, including for settings which still name the backend "rodio".
pub(super) enum DeviceOutput {
    Standard(Box<fastframe_audio::Output<MixerRender>>),
    #[cfg(target_os = "macos")]
    Apple(macos::Output),
}

impl DeviceOutput {
    pub(super) fn open(
        preferred: Option<&str>,
        buffer_ms: u32,
        render: impl Fn() -> MixerRender,
    ) -> Result<Self, OpenError> {
        #[cfg(target_os = "macos")]
        if macos::supported() {
            match macos::Output::open(preferred, render()) {
                Ok(device) => return Ok(Self::Apple(device)),
                Err(error) => {
                    log::warn!("native audio output unavailable; using standard output: {error}");
                }
            }
        }
        Ok(Self::Standard(Box::new(fastframe_audio::Output::open(
            output_options(preferred, buffer_ms),
            render(),
        )?)))
    }

    pub(super) fn set_queue(&self, queued: Arc<Queued>) {
        #[cfg(target_os = "macos")]
        if let Self::Apple(device) = self {
            device.set_queue(queued);
        }
        #[cfg(not(target_os = "macos"))]
        let _ = queued;
    }

    pub(super) fn resume(&mut self) {
        match self {
            Self::Standard(device) => device.resume(),
            #[cfg(target_os = "macos")]
            Self::Apple(device) => device.resume(),
        }
    }

    pub(super) fn pause(&mut self, deadline: Instant) {
        match self {
            Self::Standard(device) => device.pause(),
            #[cfg(target_os = "macos")]
            Self::Apple(device) => device.pause(deadline),
        }
        #[cfg(not(target_os = "macos"))]
        let _ = deadline;
    }

    pub(super) fn flush(&mut self) {
        #[cfg(target_os = "macos")]
        if let Self::Apple(device) = self {
            device.reset_handle().flush(false);
        }
    }

    #[cfg(target_os = "macos")]
    pub(super) fn reset_handle(&self) -> Option<macos::Reset> {
        match self {
            Self::Standard(_) => None,
            Self::Apple(device) => Some(device.reset_handle()),
        }
    }

    pub(super) fn failed(&self) -> bool {
        match self {
            Self::Standard(device) => device.failed(),
            #[cfg(target_os = "macos")]
            Self::Apple(device) => device.failed(),
        }
    }

    pub(super) fn device_name(&self) -> String {
        match self {
            Self::Standard(device) => device.device_name().to_string(),
            #[cfg(target_os = "macos")]
            Self::Apple(device) => device.device_name().to_string(),
        }
    }

    pub(super) fn maintain(&mut self) -> Result<(), OpenError> {
        let device = match self {
            Self::Standard(device) => device,
            #[cfg(target_os = "macos")]
            Self::Apple(device) => return device.maintain().map_err(OpenError::Native),
        };
        for error in device.take_errors() {
            if error.is_fatal() {
                log::error!("audio stream error: {error}");
            } else {
                log::warn!("audio stream error: {error}");
            }
        }
        match device.maintain() {
            Maintained::Reopened {
                device,
                sample_rate,
                reason,
                ..
            } => match reason {
                // A stream can stop asking for sound without reporting an
                // error, and then nothing would ever make room (#731).
                Reason::Stalled => log::warn!(
                    "the audio output stopped playing; opened it again: {device} at {sample_rate} Hz"
                ),
                Reason::Failed | Reason::DefaultChanged | Reason::Resumed => {
                    log::info!("audio output reopened ({reason:?}): {device} at {sample_rate} Hz")
                }
            },
            Maintained::Failed(error) => return Err(error.into()),
            Maintained::Unchanged | Maintained::Released => {}
        }
        Ok(())
    }

    #[cfg(test)]
    pub(super) fn is_paused(&self) -> bool {
        match self {
            Self::Standard(device) => device.is_paused(),
            #[cfg(target_os = "macos")]
            Self::Apple(device) => device.is_paused(),
        }
    }
}
