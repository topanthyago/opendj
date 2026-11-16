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
    queue: rtrb::Producer<(u32, [f32; 2], bool)>,
    state: std::sync::Arc<std::sync::atomic::AtomicU64>,
    config: std::sync::Arc<CaptureConfig>,
    timing: CaptureTiming,
}

pub struct CaptureHandle {
    queue: rtrb::Consumer<(u32, [f32; 2], bool)>,
    state: std::sync::Arc<std::sync::atomic::AtomicU64>,
    frames: Vec<[f32; 2]>,
    beat_markers: Vec<usize>,
    expected: usize,
    rate: u32,
    timed: bool,
    config: std::sync::Arc<CaptureConfig>,
}

#[derive(Default)]
struct CaptureConfig {
    beats: std::sync::atomic::AtomicU32,
    stem: std::sync::atomic::AtomicUsize,
    next_bar: std::sync::atomic::AtomicBool,
    recording: std::sync::atomic::AtomicBool,
    progress: std::sync::atomic::AtomicU32,
}

#[derive(Default)]
struct CaptureTiming {
    generation: u32,
    initialized: bool,
    wait: f64,
    elapsed: f64,
    identity: usize,
}

pub fn channel(rate: u32) -> (CaptureTap, CaptureHandle) {
    use std::sync::atomic::AtomicU64;
    use std::sync::Arc;
    // About five seconds at 48 kHz; the UI drains it while recording.
    let (producer, consumer) = rtrb::RingBuffer::new(262_144);
    let state = Arc::new(AtomicU64::new(0));
    let config = Arc::new(CaptureConfig::default());
    (
        CaptureTap {
            queue: producer,
            state: state.clone(),
            config: config.clone(),
            timing: CaptureTiming::default(),
        },
        CaptureHandle {
            queue: consumer,
            state,
            frames: Vec::new(),
            beat_markers: Vec::new(),
            expected: 0,
            rate,
            timed: false,
            config,
        },
    )
}

// Low 32 bits contain the remaining count or the overflow marker. The upper
// bits identify the take; frames carry the same token across cancellation.
const CAPTURE_OVERFLOW: u32 = 1 << 31;
const CAPTURE_SOURCE_LOST: u32 = CAPTURE_OVERFLOW | 1;
const CAPTURE_TOO_LONG: u32 = CAPTURE_OVERFLOW | 2;

impl CaptureTap {
    /// The selected stem while armed. Zero means the audible deck mix.
    pub fn requested_stem(&self) -> Option<usize> {
        use std::sync::atomic::Ordering;
        let count = self.state.load(Ordering::Acquire) as u32;
        (count > 0 && count < CAPTURE_OVERFLOW).then(|| self.config.stem.load(Ordering::Relaxed))
    }

    /// 0 idle/completed, 1 waiting for the source bar, 2 recording.
    pub fn capture_state(&self) -> u8 {
        use std::sync::atomic::Ordering;
        if self.requested_stem().is_none() {
            0
        } else if self.config.recording.load(Ordering::Acquire) {
            2
        } else {
            1
        }
    }

    fn fail(&self, state: u64, reason: u32) {
        use std::sync::atomic::Ordering;
        let _ = self.state.compare_exchange(
            state,
            (state & !u64::from(u32::MAX)) | u64::from(reason),
            Ordering::AcqRel,
            Ordering::Acquire,
        );
        self.config.recording.store(false, Ordering::Release);
    }

    /// Beat-counted recording of the performed audio, with no callback allocation.
    /// Tempo changes affect the duration. Missing/changed stems fail the take.
    pub fn push_clocked(
        &mut self,
        frame: [f32; 2],
        clock: Option<(f64, f64)>,
        ready: bool,
        identity: usize,
    ) {
        use std::sync::atomic::Ordering;
        let state = self.state.load(Ordering::Acquire);
        let count = state as u32;
        if count == 0 || count >= CAPTURE_OVERFLOW {
            return;
        }
        let beats = self.config.beats.load(Ordering::Relaxed);
        if beats == 0 {
            self.push(frame);
            return;
        }
        let Some((beat, per_frame)) = clock.filter(|(beat, per_frame)| {
            beat.is_finite() && per_frame.is_finite() && *per_frame > 0.0
        }) else {
            self.fail(state, CAPTURE_SOURCE_LOST);
            return;
        };
        if !ready {
            self.fail(state, CAPTURE_SOURCE_LOST);
            return;
        }
        let generation = (state >> 32) as u32;
        if !self.timing.initialized || self.timing.generation != generation {
            let phase = beat.rem_euclid(4.0);
            self.timing = CaptureTiming {
                generation,
                initialized: true,
                wait: if self.config.next_bar.load(Ordering::Relaxed) {
                    4.0 - phase
                } else {
                    0.0
                },
                elapsed: 0.0,
                identity,
            };
        }
        if self.timing.identity != identity {
            self.fail(state, CAPTURE_SOURCE_LOST);
            return;
        }
        if self.timing.wait > per_frame * 0.5 {
            self.timing.wait -= per_frame;
            return;
        }
        self.config.recording.store(true, Ordering::Release);
        if self.timing.elapsed + per_frame * 0.5 >= f64::from(beats) {
            self.fail(state, 0);
            return;
        }
        if count == 1 {
            self.fail(state, CAPTURE_TOO_LONG);
            return;
        }
        let beat_marker = self.timing.elapsed < per_frame * 0.5
            || (self.timing.elapsed + per_frame * 0.5).floor()
                > (self.timing.elapsed - per_frame * 0.5).floor();
        self.push_marked(frame, beat_marker);
        self.timing.elapsed += per_frame;
        self.config.progress.store(
            ((self.timing.elapsed / f64::from(beats)).clamp(0.0, 1.0) * 1000.0).round() as u32,
            Ordering::Relaxed,
        );
    }

    #[inline]
    pub fn push(&mut self, frame: [f32; 2]) {
        self.push_marked(frame, false);
    }

    fn push_marked(&mut self, frame: [f32; 2], beat_marker: bool) {
        use std::sync::atomic::Ordering;
        let state = self.state.load(Ordering::Acquire);
        let remaining = state as u32;
        if remaining == 0 || remaining >= CAPTURE_OVERFLOW {
            return;
        }
        let generation = (state >> 32) as u32;
        let next = if self.queue.push((generation, frame, beat_marker)).is_ok() {
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
        self.arm(frames, 0, 0, false)
    }

    /// Arm a musical duration. The callback starts at the next source bar when
    /// requested and counts the changing performed tempo through the take.
    pub fn start_beats(&mut self, beats: u32, stem: usize, next_bar: bool) -> Result<(), String> {
        if !(1..=256).contains(&beats) || stem > 4 {
            return Err("invalid beat count or stem".into());
        }
        self.arm(self.rate as usize * 128, beats, stem, next_bar)
    }

    pub fn progress(&self) -> (bool, f64) {
        use std::sync::atomic::Ordering;
        (
            self.config.recording.load(Ordering::Acquire),
            self.config.progress.load(Ordering::Relaxed) as f64 / 1000.0,
        )
    }

    fn arm(
        &mut self,
        frames: usize,
        beats: u32,
        stem: usize,
        next_bar: bool,
    ) -> Result<(), String> {
        use std::sync::atomic::Ordering;
        if self.expected != 0 {
            return Err("capture already in progress".into());
        }
        if frames == 0 || frames > self.rate as usize * 128 || frames >= CAPTURE_OVERFLOW as usize {
            return Err("invalid live capture length".into());
        }
        while self.queue.pop().is_ok() {}
        self.frames.clear();
        self.beat_markers.clear();
        self.beat_markers.reserve(beats as usize);
        self.frames.reserve(frames.min(self.rate as usize * 8));
        self.expected = frames;
        self.timed = beats > 0;
        self.config.beats.store(beats, Ordering::Relaxed);
        self.config.stem.store(stem, Ordering::Relaxed);
        self.config.next_bar.store(next_bar, Ordering::Relaxed);
        self.config.recording.store(false, Ordering::Release);
        self.config.progress.store(0, Ordering::Relaxed);
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
        self.config.recording.store(false, Ordering::Release);
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
        while let Ok((token, frame, beat_marker)) = self.queue.pop() {
            if u64::from(token) == generation {
                if beat_marker {
                    self.beat_markers.push(self.frames.len());
                }
                self.frames.push(frame);
            }
        }
        let state = self.state.load(Ordering::Acquire);
        let remaining = state as u32;
        if remaining != 0 && remaining < CAPTURE_OVERFLOW {
            return Ok(None);
        }
        // The producer publishes its final queue entry before setting remaining
        // to zero, so a second drain after the acquire sees the complete take.
        while let Ok((token, frame, beat_marker)) = self.queue.pop() {
            if u64::from(token) == generation {
                if beat_marker {
                    self.beat_markers.push(self.frames.len());
                }
                self.frames.push(frame);
            }
        }
        let expected = std::mem::take(&mut self.expected);
        self.config.recording.store(false, Ordering::Release);
        if remaining >= CAPTURE_OVERFLOW
            || self.frames.is_empty()
            || (!self.timed && self.frames.len() != expected)
        {
            self.frames.clear();
            return Err(match remaining {
                CAPTURE_SOURCE_LOST => {
                    "Capture source stopped, changed or its selected stem is not ready"
                }
                CAPTURE_TOO_LONG => "Capture exceeded the 128 second limit",
                _ => "Live capture overflowed; try again",
            }
            .into());
        }
        Ok(Some(std::mem::take(&mut self.frames)))
    }

    /// Frame positions of the performed beats from the just-completed take.
    pub fn take_beat_markers(&mut self) -> Vec<usize> {
        std::mem::take(&mut self.beat_markers)
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
    fn musical_capture_waits_for_bar_counts_tempo_changes_and_marks_beats() {
        let (mut tap, mut handle) = channel(100);
        handle.start_beats(16, 2, true).unwrap();
        let mut beat = 0.5;
        let mut output = None;
        for frame in 0..2000 {
            let per_frame = if beat < 12.0 { 0.02 } else { 0.01 };
            tap.push_clocked([frame as f32, 0.0], Some((beat, per_frame)), true, 7);
            beat += per_frame;
            if frame % 20 == 0 {
                if let Some(frames) = handle.poll().unwrap() {
                    output = Some(frames);
                    break;
                }
            }
        }
        let output = output.unwrap();
        assert_eq!(output[0][0], 175.0);
        assert!((output.len() as i64 - 1200).abs() <= 1, "{}", output.len());
        let markers = handle.take_beat_markers();
        assert_eq!(markers.len(), 16);
        assert_eq!(markers[0], 0);
        assert!(markers.windows(2).all(|pair| pair[0] < pair[1]));
    }

    #[test]
    fn musical_capture_rejects_missing_and_changed_sources() {
        let (mut tap, mut handle) = channel(100);
        handle.start_beats(4, 1, false).unwrap();
        tap.push_clocked([1.0; 2], Some((0.0, 0.02)), false, 1);
        assert!(handle.poll().unwrap_err().contains("stem"));
        handle.start_beats(4, 1, false).unwrap();
        tap.push_clocked([1.0; 2], Some((0.0, 0.02)), true, 1);
        tap.push_clocked([1.0; 2], Some((0.02, 0.02)), true, 2);
        assert!(handle.poll().is_err());
        handle.start_beats(4, 1, false).unwrap();
        tap.push_clocked([1.0; 2], None, true, 2);
        assert!(handle.poll().is_err());
    }

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
        tap.queue.push((old as u32, [9.0, 9.0], false)).unwrap();
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
