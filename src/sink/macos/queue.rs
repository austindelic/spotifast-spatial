//! Timestamped PCM retained until playback, including samples needed for recovery.

use std::collections::VecDeque;

use librespot_playback::SAMPLE_RATE;

use super::CHANNELS;

pub(super) const BLOCK: u64 = SAMPLE_RATE as u64 / 50;
pub(super) const LOOKAHEAD: u64 = SAMPLE_RATE as u64 / 4;
// Apple's preroll threshold is strictly greater than 500 ms. One extra frame
// adds 22.7 microseconds at 44.1 kHz without growing the reservoir further.
pub(super) const MAX_LOOKAHEAD: u64 = SAMPLE_RATE as u64 / 2 + 1;

pub(super) struct Chunk {
    pub(super) start: u64,
    pub(super) pcm: Vec<f32>,
}

impl Chunk {
    pub(super) fn end(&self) -> u64 {
        self.start + (self.pcm.len() / CHANNELS) as u64
    }
}

#[derive(Default)]
pub(super) struct Queue {
    pub(super) next: u64,
    pub(super) pending: VecDeque<Chunk>,
}

impl Queue {
    pub(super) fn played(&mut self, frame: u64) {
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

    pub(super) fn frames_to_fill(&self, frame: u64, lookahead: u64) -> u64 {
        frame
            .saturating_add(lookahead.min(MAX_LOOKAHEAD))
            .saturating_sub(self.next)
            .min(BLOCK)
    }

    pub(super) fn trim(&mut self, frame: u64) {
        self.played(frame);
        if let Some(first) = self.pending.front_mut()
            && first.start < frame
        {
            first.pcm.drain(..(frame - first.start) as usize * CHANNELS);
            first.start = frame;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
}
