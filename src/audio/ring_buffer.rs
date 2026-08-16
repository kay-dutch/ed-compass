//! Fixed-capacity multichannel PCM ring buffer.
//!
//! Storage is interleaved, matching what WASAPI hands us, so the capture thread
//! can append with at most two `copy_from_slice` calls and no per-sample work.
//! Nothing is ever reallocated and the whole buffer is never copied.
//!
//! The ring also carries the capture timeline: `total_frames` counts every frame
//! ever written, including silence synthesized across a device gap. Absolute
//! frame indices derived from it stay meaningful for the life of the stream,
//! which is what lets a triggered capture ask for "the 30 seconds before this
//! event" long after those samples were written.

/// Why a range read could not be served.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum RangeError {
    /// The requested span begins before the oldest frame still resident.
    #[error("range starts at frame {requested} but the oldest resident frame is {oldest}")]
    Evicted { requested: u64, oldest: u64 },
    /// The requested span extends past what has been written.
    #[error("range ends at frame {requested_end} but only {total} frames have been written")]
    NotYetWritten { requested_end: u64, total: u64 },
}

#[derive(Debug)]
pub struct PcmRing {
    /// `capacity_frames * channels` samples, interleaved.
    data: Vec<f32>,
    channels: usize,
    capacity_frames: usize,
    /// Frame index within `data` where the next frame will be written.
    write_frame: usize,
    /// Frames currently valid, saturating at `capacity_frames`.
    len_frames: usize,
    /// Every frame ever written. Never wraps in any realistic session.
    total_frames: u64,
}

impl PcmRing {
    pub fn new(capacity_frames: usize, channels: usize) -> Self {
        assert!(capacity_frames > 0, "ring capacity must be non-zero");
        assert!(channels > 0, "ring must have at least one channel");
        Self {
            data: vec![0.0; capacity_frames * channels],
            channels,
            capacity_frames,
            write_frame: 0,
            len_frames: 0,
            total_frames: 0,
        }
    }

    /// Size a ring from a duration, rounding up so the requested window always fits.
    pub fn with_seconds(seconds: f32, sample_rate: u32, channels: usize) -> Self {
        let frames = (seconds * sample_rate as f32).ceil().max(1.0) as usize;
        Self::new(frames, channels)
    }

    pub fn channels(&self) -> usize {
        self.channels
    }

    pub fn capacity_frames(&self) -> usize {
        self.capacity_frames
    }

    pub fn len_frames(&self) -> usize {
        self.len_frames
    }

    pub fn is_empty(&self) -> bool {
        self.len_frames == 0
    }

    pub fn is_full(&self) -> bool {
        self.len_frames == self.capacity_frames
    }

    /// Total frames ever written, including synthesized silence. This is the
    /// capture timeline.
    pub fn total_frames(&self) -> u64 {
        self.total_frames
    }

    /// Absolute index of the oldest frame still resident.
    pub fn oldest_frame(&self) -> u64 {
        self.total_frames - self.len_frames as u64
    }

    pub fn bytes(&self) -> usize {
        self.data.len() * std::mem::size_of::<f32>()
    }

    /// Append interleaved frames, evicting the oldest as needed.
    ///
    /// Panics if `samples` is not a whole number of frames — that would desync
    /// the channel interleave for every subsequent read, so it must never be
    /// papered over.
    pub fn push_interleaved(&mut self, samples: &[f32]) {
        assert_eq!(
            samples.len() % self.channels,
            0,
            "push_interleaved got {} samples, not a multiple of {} channels",
            samples.len(),
            self.channels
        );
        let frames = samples.len() / self.channels;
        if frames == 0 {
            return;
        }

        // More than a full ring in one go: only the tail can survive, so skip
        // straight to it rather than copying data we would immediately evict.
        let (samples, frames, skipped) = if frames >= self.capacity_frames {
            let skipped = frames - self.capacity_frames;
            (
                &samples[skipped * self.channels..],
                self.capacity_frames,
                skipped,
            )
        } else {
            (samples, frames, 0)
        };

        let first = (self.capacity_frames - self.write_frame).min(frames);
        let head = self.write_frame * self.channels;
        self.data[head..head + first * self.channels]
            .copy_from_slice(&samples[..first * self.channels]);

        let rest = frames - first;
        if rest > 0 {
            self.data[..rest * self.channels].copy_from_slice(&samples[first * self.channels..]);
        }

        self.write_frame = (self.write_frame + frames) % self.capacity_frames;
        self.len_frames = (self.len_frames + frames).min(self.capacity_frames);
        self.total_frames += (frames + skipped) as u64;
    }

    /// Append `frames` of silence. Used to hold the timeline together across a
    /// WASAPI discontinuity or an idle loopback endpoint — see `capture.rs`.
    pub fn push_silence(&mut self, frames: usize) {
        if frames == 0 {
            return;
        }
        let to_write = frames.min(self.capacity_frames);
        let mut remaining = to_write;
        while remaining > 0 {
            let chunk = remaining.min(self.capacity_frames - self.write_frame);
            let head = self.write_frame * self.channels;
            self.data[head..head + chunk * self.channels].fill(0.0);
            self.write_frame = (self.write_frame + chunk) % self.capacity_frames;
            remaining -= chunk;
        }
        self.len_frames = (self.len_frames + to_write).min(self.capacity_frames);
        // The timeline advances by the full request even if the ring could only
        // hold the tail of it.
        self.total_frames += frames as u64;
    }

    /// The resident samples as two interleaved slices in chronological order.
    /// The second is empty when the data does not wrap.
    pub fn slices(&self) -> (&[f32], &[f32]) {
        if self.len_frames == 0 {
            return (&[], &[]);
        }
        let start_frame =
            (self.write_frame + self.capacity_frames - self.len_frames) % self.capacity_frames;
        let start = start_frame * self.channels;
        let end = start + self.len_frames * self.channels;
        if end <= self.data.len() {
            (&self.data[start..end], &[])
        } else {
            (&self.data[start..], &self.data[..end - self.data.len()])
        }
    }

    /// Copy an absolute frame range out, appending to `out`. This is how a
    /// triggered capture extracts its pre-roll.
    pub fn copy_range(
        &self,
        start_frame: u64,
        frames: usize,
        out: &mut Vec<f32>,
    ) -> Result<(), RangeError> {
        let oldest = self.oldest_frame();
        if start_frame < oldest {
            return Err(RangeError::Evicted {
                requested: start_frame,
                oldest,
            });
        }
        let end = start_frame + frames as u64;
        if end > self.total_frames {
            return Err(RangeError::NotYetWritten {
                requested_end: end,
                total: self.total_frames,
            });
        }
        if frames == 0 {
            return Ok(());
        }

        let offset = (start_frame - oldest) as usize;
        let (a, b) = self.slices();
        let a_frames = a.len() / self.channels;

        out.reserve(frames * self.channels);
        let from_a = frames.min(a_frames.saturating_sub(offset));
        if from_a > 0 {
            let s = offset * self.channels;
            out.extend_from_slice(&a[s..s + from_a * self.channels]);
        }
        let from_b = frames - from_a;
        if from_b > 0 {
            let s = offset.saturating_sub(a_frames) * self.channels;
            out.extend_from_slice(&b[s..s + from_b * self.channels]);
        }
        Ok(())
    }

    /// Copy the most recent `frames` (clamped to what is resident).
    pub fn copy_latest(&self, frames: usize, out: &mut Vec<f32>) -> usize {
        let n = frames.min(self.len_frames);
        let start = self.total_frames - n as u64;
        // Cannot fail: the range is derived from the ring's own state.
        self.copy_range(start, n, out)
            .expect("copy_latest range is always resident");
        n
    }

    /// Iterate one channel of the resident data, oldest first.
    pub fn channel_iter(&self, channel: usize) -> impl Iterator<Item = f32> + '_ {
        assert!(channel < self.channels, "channel {channel} out of range");
        let (a, b) = self.slices();
        let ch = self.channels;
        a.iter()
            .skip(channel)
            .step_by(ch)
            .chain(b.iter().skip(channel).step_by(ch))
            .copied()
    }

    /// Drop all resident audio but keep the timeline. Used when the stream
    /// format changes and the old samples are no longer interpretable.
    pub fn clear(&mut self) {
        self.len_frames = 0;
        self.write_frame = 0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Interleaved ramp: frame i, channel c => i * 10 + c.
    fn ramp(frames: usize, channels: usize, start: usize) -> Vec<f32> {
        (0..frames)
            .flat_map(|i| (0..channels).map(move |c| ((start + i) * 10 + c) as f32))
            .collect()
    }

    fn resident(ring: &PcmRing) -> Vec<f32> {
        let (a, b) = ring.slices();
        a.iter().chain(b.iter()).copied().collect()
    }

    #[test]
    fn starts_empty() {
        let ring = PcmRing::new(10, 2);
        assert!(ring.is_empty());
        assert_eq!(ring.len_frames(), 0);
        assert_eq!(ring.total_frames(), 0);
        assert_eq!(ring.slices(), (&[][..], &[][..]));
    }

    #[test]
    fn sizes_from_seconds() {
        let ring = PcmRing::with_seconds(150.0, 48_000, 8);
        assert_eq!(ring.capacity_frames(), 150 * 48_000);
        assert_eq!(ring.bytes(), 150 * 48_000 * 8 * 4);
    }

    #[test]
    fn partial_fill_reads_back_verbatim() {
        let mut ring = PcmRing::new(10, 2);
        ring.push_interleaved(&ramp(4, 2, 0));
        assert_eq!(ring.len_frames(), 4);
        assert_eq!(ring.total_frames(), 4);
        assert_eq!(resident(&ring), ramp(4, 2, 0));
    }

    #[test]
    fn discards_oldest_on_overflow() {
        let mut ring = PcmRing::new(4, 2);
        ring.push_interleaved(&ramp(6, 2, 0));
        assert!(ring.is_full());
        assert_eq!(ring.len_frames(), 4);
        assert_eq!(ring.total_frames(), 6);
        // Frames 0 and 1 are gone; 2..=5 remain, in order.
        assert_eq!(resident(&ring), ramp(4, 2, 2));
        assert_eq!(ring.oldest_frame(), 2);
    }

    #[test]
    fn oversized_push_keeps_only_the_tail() {
        let mut ring = PcmRing::new(3, 1);
        ring.push_interleaved(&ramp(100, 1, 0));
        assert_eq!(ring.len_frames(), 3);
        assert_eq!(ring.total_frames(), 100);
        assert_eq!(resident(&ring), ramp(3, 1, 97));
    }

    #[test]
    fn wraps_across_the_seam_in_order() {
        let mut ring = PcmRing::new(5, 2);
        ring.push_interleaved(&ramp(3, 2, 0));
        ring.push_interleaved(&ramp(4, 2, 3)); // wraps
        assert_eq!(ring.total_frames(), 7);
        assert_eq!(resident(&ring), ramp(5, 2, 2));
        let (a, b) = ring.slices();
        assert!(!b.is_empty(), "this case is meant to exercise the wrap");
        assert_eq!(a.len() + b.len(), 5 * 2);
    }

    #[test]
    fn many_small_pushes_stay_consistent() {
        let mut ring = PcmRing::new(7, 3);
        for i in 0..50 {
            ring.push_interleaved(&ramp(1, 3, i));
        }
        assert_eq!(ring.total_frames(), 50);
        assert_eq!(resident(&ring), ramp(7, 3, 43));
    }

    #[test]
    fn silence_advances_the_timeline() {
        let mut ring = PcmRing::new(8, 2);
        ring.push_interleaved(&ramp(2, 2, 0));
        ring.push_silence(3);
        assert_eq!(ring.total_frames(), 5);
        assert_eq!(ring.len_frames(), 5);
        let got = resident(&ring);
        assert_eq!(&got[..4], &ramp(2, 2, 0)[..]);
        assert!(got[4..].iter().all(|&s| s == 0.0));
    }

    #[test]
    fn silence_longer_than_the_ring_still_advances_the_clock() {
        let mut ring = PcmRing::new(4, 1);
        ring.push_interleaved(&ramp(4, 1, 0));
        ring.push_silence(1000);
        assert_eq!(ring.total_frames(), 1004);
        assert_eq!(ring.len_frames(), 4);
        assert!(resident(&ring).iter().all(|&s| s == 0.0));
        assert_eq!(ring.oldest_frame(), 1000);
    }

    #[test]
    fn channel_iter_extracts_one_channel() {
        let mut ring = PcmRing::new(4, 3);
        ring.push_interleaved(&ramp(6, 3, 0)); // frames 2..=5 survive
        let ch1: Vec<f32> = ring.channel_iter(1).collect();
        assert_eq!(ch1, vec![21.0, 31.0, 41.0, 51.0]);
    }

    #[test]
    fn copy_range_spans_the_wrap() {
        let mut ring = PcmRing::new(5, 2);
        ring.push_interleaved(&ramp(8, 2, 0)); // frames 3..=7 resident
        let mut out = Vec::new();
        ring.copy_range(4, 3, &mut out).unwrap();
        assert_eq!(out, ramp(3, 2, 4));
    }

    #[test]
    fn copy_range_rejects_evicted_and_future_spans() {
        let mut ring = PcmRing::new(4, 1);
        ring.push_interleaved(&ramp(10, 1, 0)); // frames 6..=9 resident
        let mut out = Vec::new();
        assert_eq!(
            ring.copy_range(2, 2, &mut out),
            Err(RangeError::Evicted {
                requested: 2,
                oldest: 6
            })
        );
        assert_eq!(
            ring.copy_range(8, 5, &mut out),
            Err(RangeError::NotYetWritten {
                requested_end: 13,
                total: 10
            })
        );
        assert!(out.is_empty(), "failed reads must not append anything");
    }

    #[test]
    fn copy_latest_clamps_to_what_is_resident() {
        let mut ring = PcmRing::new(4, 2);
        ring.push_interleaved(&ramp(3, 2, 0));
        let mut out = Vec::new();
        assert_eq!(ring.copy_latest(100, &mut out), 3);
        assert_eq!(out, ramp(3, 2, 0));
    }

    #[test]
    fn empty_push_is_a_no_op() {
        let mut ring = PcmRing::new(4, 2);
        ring.push_interleaved(&[]);
        ring.push_silence(0);
        assert_eq!(ring.total_frames(), 0);
        assert!(ring.is_empty());
    }

    #[test]
    #[should_panic(expected = "not a multiple of")]
    fn partial_frame_push_panics() {
        let mut ring = PcmRing::new(4, 2);
        ring.push_interleaved(&[1.0, 2.0, 3.0]);
    }

    #[test]
    fn clear_keeps_the_timeline() {
        let mut ring = PcmRing::new(4, 1);
        ring.push_interleaved(&ramp(6, 1, 0));
        ring.clear();
        assert!(ring.is_empty());
        assert_eq!(ring.total_frames(), 6);
        assert_eq!(ring.oldest_frame(), 6);
    }
}
