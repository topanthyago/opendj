//! A prepared sampling take shared by the GUI and the SDK controller surface.
//! It never enters the project or Master until the explicit Apply operation.
use super::{MaschineHandle, Sample};
use std::sync::Arc;

pub struct SamplingPreview {
    pub project_generation: u64,
    pub sample: Arc<Sample>,
    /// Inclusive frame ranges, one for each audition pad.
    pub ranges: Vec<(usize, usize)>,
    pub tunes: [f64; 16],
    pub gains: [f64; 16],
}

impl MaschineHandle {
    pub fn set_sampling_preview(&self, preview: Option<SamplingPreview>) -> Result<(), String> {
        if preview.as_ref().is_some_and(|preview| {
            preview.ranges.is_empty()
                || preview.ranges.len() > 16
                || preview
                    .ranges
                    .iter()
                    .any(|(start, end)| start > end || *end >= preview.sample.frames.len())
                || preview.tunes.iter().any(|tune| !tune.is_finite())
                || preview
                    .gains
                    .iter()
                    .any(|gain| !gain.is_finite() || *gain < 0.0)
        }) {
            return Err("Invalid prepared sampling preview".into());
        }
        if preview
            .as_ref()
            .is_some_and(|preview| preview.project_generation != self.project_generation())
        {
            return Err("Project changed while preparing the sample".into());
        }
        *self
            .shared
            .sampling_preview
            .write()
            .map_err(|_| "Sampling preview unavailable")? = preview.map(Arc::new);
        Ok(())
    }

    pub fn sampling_preview(&self) -> Option<Arc<SamplingPreview>> {
        self.shared
            .sampling_preview
            .read()
            .ok()?
            .as_ref()
            .filter(|preview| preview.project_generation == self.project_generation())
            .cloned()
    }

    /// SDK Sampling pads address the same prepared ranges as the GUI. Tuning
    /// and copying happen on the surface worker before the Cue queue is used.
    pub fn preview_staged(&self, start: usize) -> Result<(), String> {
        let preview = self.sampling_preview().ok_or("Prepared capture changed")?;
        let index = preview
            .ranges
            .iter()
            .position(|range| range.0 == start)
            .ok_or("Prepared slice changed. Retry the pad")?;
        let (start, end) = preview.ranges[index];
        let frames = preview
            .sample
            .frames
            .get(start..=end)
            .ok_or("Invalid slice range")?
            .iter()
            .map(|frame| frame.map(|sample| sample * preview.gains[index] as f32))
            .collect();
        let rate = (preview.sample.rate as f64 * 2.0f64.powf(preview.tunes[index] / 12.0))
            .round()
            .clamp(1.0, u32::MAX as f64) as u32;
        self.preview_audio(Arc::new(Sample { frames, rate }))
    }
}
