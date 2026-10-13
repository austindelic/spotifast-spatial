//! Deterministic stereo PCM and integer-frame scheduling, without Apple APIs.

pub const SAMPLE_RATE: u32 = 44_100;
pub const CHANNELS: usize = 2;
pub const BLOCK_FRAMES: u64 = 882;
pub const LOOKAHEAD_FRAMES: u64 = BLOCK_FRAMES * 2;

/// A chunk is scheduled only if its end fits inside the bounded lookahead.
pub fn next_chunk(next: u64, played: u64, end: u64) -> Option<u64> {
    let count = BLOCK_FRAMES.min(end.saturating_sub(next));
    (count > 0 && next + count <= played.saturating_add(LOOKAHEAD_FRAMES)).then_some(count)
}

/// Quiet alternating arpeggios make stereo spatialization easy to compare.
/// Phases and envelopes derive from absolute frames, including at chunk edges.
pub fn samples(start: u64, frames: u64) -> Vec<f32> {
    const NOTES: [f64; 8] = [220.0, 261.63, 329.63, 440.0, 329.63, 261.63, 293.66, 392.0];
    let note_frames = u64::from(SAMPLE_RATE) / 2;
    let mut samples = Vec::with_capacity(frames as usize * CHANNELS);
    for frame in start..start + frames {
        let note = (frame / note_frames) as usize % NOTES.len();
        let within = (frame % note_frames) as f64 / f64::from(SAMPLE_RATE);
        let envelope = (within / 0.02).min(1.0) * ((0.5 - within) / 0.08).min(1.0);
        let tone = (within * NOTES[note] * std::f64::consts::TAU).sin() * envelope * 0.08;
        let gains = if note.is_multiple_of(2) {
            [1.0, 0.3]
        } else {
            [0.3, 1.0]
        };
        for gain in gains {
            samples.push((tone * gain) as f32);
        }
    }
    samples
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scheduling_never_exceeds_forty_ms() {
        let end = u64::from(SAMPLE_RATE) * 120;
        let mut next = 0;
        for played in (0..end).step_by(221) {
            while let Some(count) = next_chunk(next, played, end) {
                next += count;
                assert!(next <= played + LOOKAHEAD_FRAMES);
                assert!(next <= end);
            }
        }
        assert_eq!(next, end);
    }

    #[test]
    fn a_full_queue_waits_and_the_final_chunk_is_short() {
        assert_eq!(next_chunk(0, 0, 2_000), Some(882));
        assert_eq!(next_chunk(882, 0, 2_000), Some(882));
        assert_eq!(next_chunk(1_764, 0, 2_000), None);
        assert_eq!(next_chunk(1_764, 882, 2_000), Some(236));
        assert_eq!(next_chunk(2_000, 2_000, 2_000), None);
    }

    #[test]
    fn signal_is_contiguous_stereo_and_quiet() {
        let entire = samples(0, 1_764);
        let mut chunks = samples(0, 882);
        chunks.extend(samples(882, 882));
        assert_eq!(chunks, entire);
        assert_eq!(entire.len(), 3_528);
        assert!(entire.iter().all(|x| x.is_finite() && x.abs() <= 0.08));
        assert!(
            entire
                .as_chunks::<2>()
                .0
                .iter()
                .any(|frame| frame[0] != frame[1])
        );
    }
}
