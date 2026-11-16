//! Off-callback extraction and slicing of decoded deck audio.
use crate::stems::MAX_STEMS;
use crate::track::Audio;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Stem {
    Mix,
    Drums,
    Bass,
    Melody,
    Voice,
}

#[derive(Clone, Debug)]
pub enum SliceMode {
    Grid,
    Transients,
    Manual(Vec<usize>),
}

/// Extract 4/4 bars from an explicit frame and tempo. Never substitutes an
/// unprocessed stem with silence or starts an expensive separation job.
pub fn capture(
    audio: &Audio,
    stem: Stem,
    start: usize,
    bpm: f64,
    bars: u32,
) -> Result<Vec<[f32; 2]>, String> {
    if !bpm.is_finite()
        || !(20.0..=400.0).contains(&bpm)
        || !(1..=64).contains(&bars)
        || audio.sample_rate == 0
    {
        return Err("invalid tempo, bar count or sample rate".into());
    }
    let frames = (audio.sample_rate as f64 * 60.0 / bpm * 4.0 * bars as f64).round() as usize;
    let end = start.checked_add(frames).ok_or("capture range overflow")?;
    if end > audio.len() || end > audio.decoded() {
        return Err("capture extends beyond decoded audio".into());
    }
    if stem == Stem::Mix {
        return Ok((start..end).map(|i| audio.frame(i)).collect());
    }
    let stems = audio.stems().ok_or("stems are not ready")?.current();
    let mut weights = [0.0; MAX_STEMS];
    for (i, info) in stems.info.iter().take(MAX_STEMS).enumerate() {
        let name = info.name.to_lowercase();
        let selected = match stem {
            Stem::Drums => ["drum", "kick", "hihat", "hi-hat"]
                .iter()
                .any(|s| name.contains(s)),
            Stem::Bass => name.contains("bass"),
            Stem::Melody => ["instru", "other", "melody", "synth"]
                .iter()
                .any(|s| name.contains(s)),
            Stem::Voice => name.contains("vocal") || name.contains("voice"),
            Stem::Mix => false,
        };
        if selected {
            weights[i] = 1.0;
        }
    }
    if weights.iter().all(|x| *x == 0.0) {
        return Err("selected stem is unavailable".into());
    }
    if (start..end).any(|i| !stems.has_frame(i)) {
        return Err("selected stem is still processing this range".into());
    }
    Ok((start..end)
        .map(|i| stems.solo_blend(None, 0.0, i, &weights))
        .collect())
}

/// Sixteen nonempty slices, expressed as frame boundaries.
pub fn slices(frames: &[[f32; 2]], mode: &SliceMode) -> Result<Vec<usize>, String> {
    let n = frames.len();
    if n < 16 {
        return Err("sample is too short to slice".into());
    }
    let mut boundaries: Vec<usize> = (0..=16).map(|i| i * n / 16).collect();
    match mode {
        SliceMode::Grid => (),
        SliceMode::Manual(points) => {
            if points.len() != 17
                || points[0] != 0
                || points[16] != n
                || points.windows(2).any(|w| w[0] >= w[1])
            {
                return Err(
                    "manual slices must contain 17 increasing boundaries from 0 to the sample end"
                        .into(),
                );
            }
            boundaries.clone_from(points);
        }
        SliceMode::Transients => {
            // One onset per grid neighborhood keeps all 16 pads useful.
            let radius = (n / 64).max(1);
            for i in 1..16 {
                let center = i * n / 16;
                let lo = center.saturating_sub(radius).max(boundaries[i - 1] + 1);
                let hi = (center + radius).min((i + 1) * n / 16 - 1);
                if lo > hi {
                    continue;
                }
                boundaries[i] = (lo..=hi)
                    .max_by(|a, b| {
                        let onset = |p: usize| {
                            let amp = |x: [f32; 2]| x[0].abs().max(x[1].abs());
                            amp(frames[p]) - amp(frames[p.saturating_sub(1)])
                        };
                        onset(*a).total_cmp(&onset(*b))
                    })
                    .unwrap_or(center);
            }
        }
    }
    Ok(boundaries)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn capture_and_slice_bounds() {
        let audio = Audio::from_frames(100, vec![[0.5, -0.5]; 1000]);
        let out = capture(&audio, Stem::Mix, 0, 120.0, 1).unwrap();
        assert_eq!(out.len(), 200);
        assert_eq!(slices(&out, &SliceMode::Grid).unwrap().len(), 17);
        assert!(capture(&audio, Stem::Voice, 0, 120.0, 1).is_err());
        assert!(capture(&audio, Stem::Mix, 900, 120.0, 1).is_err());
        assert!(slices(&out, &SliceMode::Manual(vec![0, 200])).is_err());
        let manual: Vec<usize> = (0..=16).map(|i| i * out.len() / 16).collect();
        assert_eq!(
            slices(&out, &SliceMode::Manual(manual.clone())).unwrap(),
            manual
        );
        let transient = slices(&out, &SliceMode::Transients).unwrap();
        assert_eq!(transient.len(), 17);
        assert!(transient.windows(2).all(|w| w[0] < w[1]));
    }

    #[test]
    fn extracts_only_requested_stem() {
        let audio = Audio::from_frames(100, vec![[0.9, 0.9]; 400]);
        let mut frame = [[0.0; 2]; MAX_STEMS];
        frame[0] = [0.25, -0.25];
        frame[1] = [0.5, 0.5];
        assert!(audio.set_stems(crate::stems::Stems::from_frames(
            crate::stems::default_info(),
            100,
            &vec![frame; 400],
        )));
        let drums = capture(&audio, Stem::Drums, 0, 120.0, 1).unwrap();
        let bass = capture(&audio, Stem::Bass, 0, 120.0, 1).unwrap();
        assert!((drums[0][0] - 0.25).abs() < 0.001);
        assert!((bass[0][0] - 0.5).abs() < 0.001);
    }
}

/// A bounded transfer from the real-time deck to its non-real-time consumer.
/// `push` never allocates, blocks, or performs I/O.
pub struct CaptureTap {
    queue: rtrb::Producer<(u32, [f32; 2])>,
    state: std::sync::Arc<std::sync::atomic::AtomicU64>,
}

pub struct CaptureHandle {
    queue: rtrb::Consumer<(u32, [f32; 2])>,
    state: std::sync::Arc<std::sync::atomic::AtomicU64>,
    frames: Vec<[f32; 2]>,
    expected: usize,
    rate: u32,
}

pub fn channel(rate: u32) -> (CaptureTap, CaptureHandle) {
    use std::sync::atomic::AtomicU64;
    use std::sync::Arc;
    // About five seconds at 48 kHz; the UI drains it while recording.
    let (producer, consumer) = rtrb::RingBuffer::new(262_144);
    let state = Arc::new(AtomicU64::new(0));
    (
        CaptureTap {
            queue: producer,
            state: state.clone(),
        },
        CaptureHandle {
            queue: consumer,
            state,
            frames: Vec::new(),
            expected: 0,
            rate,
        },
    )
}

// Low 32 bits contain the remaining count or the overflow marker. The upper
// bits identify the take; frames carry the same token across cancellation.
const CAPTURE_OVERFLOW: u32 = 1 << 31;

impl CaptureTap {
    #[inline]
    pub fn push(&mut self, frame: [f32; 2]) {
        use std::sync::atomic::Ordering;
        let state = self.state.load(Ordering::Acquire);
        let remaining = state as u32;
        if remaining == 0 || remaining == CAPTURE_OVERFLOW {
            return;
        }
        let generation = (state >> 32) as u32;
        let next = if self.queue.push((generation, frame)).is_ok() {
            remaining - 1
        } else {
            CAPTURE_OVERFLOW
        };
        // A cancel/rearm invalidates this callback's state. Its tagged frame
        // can remain in the queue, but it can never count toward the new take.
        let _ = self.state.compare_exchange(
            state,
            (u64::from(generation) << 32) | u64::from(next),
            Ordering::AcqRel,
            Ordering::Acquire,
        );
    }
}

impl CaptureHandle {
    /// Arm an exact number of frames; a capture is limited to 128 seconds.
    pub fn start(&mut self, frames: usize) -> Result<(), String> {
        use std::sync::atomic::Ordering;
        if self.expected != 0 {
            return Err("capture already in progress".into());
        }
        if frames == 0 || frames > self.rate as usize * 128 || frames >= CAPTURE_OVERFLOW as usize {
            return Err("invalid live capture length".into());
        }
        while self.queue.pop().is_ok() {}
        self.frames.clear();
        self.frames.reserve(frames);
        self.expected = frames;
        let generation = (self.state.load(Ordering::Acquire) >> 32) as u32;
        let generation = generation.wrapping_add(1);
        self.state.store(
            (u64::from(generation) << 32) | frames as u64,
            Ordering::Release,
        );
        Ok(())
    }

    /// Token for the current take, to guard a worker against later takes.
    pub fn generation(&self) -> u64 {
        self.state.load(std::sync::atomic::Ordering::Acquire) >> 32
    }

    /// Cancel only the take identified by `generation`, without waiting for audio.
    pub fn cancel_generation(&mut self, generation: u64) -> bool {
        use std::sync::atomic::Ordering;
        if self.expected == 0 || self.generation() != generation {
            return false;
        }
        let next = (generation as u32).wrapping_add(1);
        self.state.store(u64::from(next) << 32, Ordering::Release);
        self.expected = 0;
        self.frames.clear();
        while self.queue.pop().is_ok() {}
        true
    }

    /// Drain only the identified take; a stale worker cannot consume a new one.
    pub fn poll_generation(&mut self, generation: u64) -> Result<Option<Vec<[f32; 2]>>, String> {
        use std::sync::atomic::Ordering;
        if self.generation() != generation {
            return Err("capture no longer current".into());
        }
        if self.expected == 0 {
            return Ok(None);
        }
        while let Ok((token, frame)) = self.queue.pop() {
            if u64::from(token) == generation {
                self.frames.push(frame);
            }
        }
        let state = self.state.load(Ordering::Acquire);
        let remaining = state as u32;
        if remaining != 0 && remaining != CAPTURE_OVERFLOW {
            return Ok(None);
        }
        // The producer publishes its final queue entry before setting remaining
        // to zero, so a second drain after the acquire sees the complete take.
        while let Ok((token, frame)) = self.queue.pop() {
            if u64::from(token) == generation {
                self.frames.push(frame);
            }
        }
        let expected = std::mem::take(&mut self.expected);
        if remaining == CAPTURE_OVERFLOW || self.frames.len() != expected {
            self.frames.clear();
            return Err("live capture overflowed; try again".into());
        }
        Ok(Some(std::mem::take(&mut self.frames)))
    }

    /// Drain regularly outside the callback. An overflow fails the entire take.
    pub fn poll(&mut self) -> Result<Option<Vec<[f32; 2]>>, String> {
        self.poll_generation(self.generation())
    }
}

#[cfg(test)]
mod live_tests {
    use super::*;
    #[test]
    fn live_capture_has_exact_frames() {
        let (mut tap, mut handle) = channel(100);
        handle.start(3).unwrap();
        tap.push([1.0, 2.0]);
        assert!(handle.poll().unwrap().is_none());
        tap.push([3.0, 4.0]);
        tap.push([5.0, 6.0]);
        tap.push([7.0, 8.0]);
        assert_eq!(
            handle.poll().unwrap().unwrap(),
            vec![[1.0, 2.0], [3.0, 4.0], [5.0, 6.0]]
        );
    }

    #[test]
    fn cancel_rearm_discards_stale_producer_and_old_worker() {
        use std::sync::atomic::Ordering;
        let (mut tap, mut handle) = channel(100);
        handle.start(2).unwrap();
        let old = handle.generation();
        // Simulate a callback paused immediately after loading the old state.
        let stale = tap.state.load(Ordering::Acquire);
        assert!(handle.cancel_generation(old));
        handle.start(2).unwrap();
        let current = handle.generation();
        assert!(!handle.cancel_generation(old));
        assert!(handle.poll_generation(old).is_err());
        tap.queue.push((old as u32, [9.0, 9.0])).unwrap();
        assert!(tap
            .state
            .compare_exchange(stale, stale - 1, Ordering::AcqRel, Ordering::Acquire)
            .is_err());
        tap.push([1.0, 2.0]);
        tap.push([3.0, 4.0]);
        assert_eq!(
            handle.poll_generation(current).unwrap().unwrap(),
            vec![[1.0, 2.0], [3.0, 4.0]]
        );
    }

    #[test]
    fn live_overflow_fails_instead_of_returning_truncated_audio() {
        let (mut tap, mut handle) = channel(10_000);
        handle.start(262_145).unwrap();
        for _ in 0..262_145 {
            tap.push([0.0, 0.0]);
        }
        assert!(handle.poll().is_err());
    }
}
