//! Deterministic energy-based speech detection.
use super::{duration_to_samples, samples_to_duration, LevelMeter};
use std::time::Duration;

/// Boundary for replacing the deterministic fallback with WebRTC VAD later.
#[cfg(test)]
pub trait VoiceActivityDetector: Send + Sync {
    fn analyze(&self, samples: &[f32], sample_rate: u32) -> VadAnalysis;
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct EnergyVadConfig {
    pub frame_duration: Duration,
    pub rms_threshold: f32,
    pub minimum_voice_duration: Duration,
}

impl Default for EnergyVadConfig {
    fn default() -> Self {
        Self {
            frame_duration: Duration::from_millis(20),
            rms_threshold: 0.01,
            minimum_voice_duration: Duration::from_millis(100),
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct VadAnalysis {
    pub contains_voice: bool,
    pub voiced_duration: Duration,
    pub trailing_silence: Duration,
}

#[derive(Clone, Copy, Debug)]
pub struct EnergyVad {
    config: EnergyVadConfig,
}

impl EnergyVad {
    pub fn new(config: EnergyVadConfig) -> Self {
        Self { config }
    }
}

#[cfg(test)]
impl VoiceActivityDetector for EnergyVad {
    fn analyze(&self, samples: &[f32], sample_rate: u32) -> VadAnalysis {
        self.analyze_with_meter(samples, sample_rate).0
    }
}

impl EnergyVad {
    /// Computes VAD and the whole-recording meter in one pass over the audio.
    pub(super) fn analyze_with_meter(
        &self,
        samples: &[f32],
        sample_rate: u32,
    ) -> (VadAnalysis, LevelMeter) {
        if samples.is_empty() || sample_rate == 0 {
            return (VadAnalysis::default(), LevelMeter::default());
        }

        let frame_len = duration_to_samples(self.config.frame_duration, sample_rate).max(1);
        let mut voiced_samples = 0usize;
        let mut trailing_silent_samples = 0usize;
        let mut seen_voice = false;
        let mut peak = 0.0f32;
        let mut total_sum_squares = 0.0f64;

        for frame in samples.chunks(frame_len) {
            let mut frame_sum_squares = 0.0f64;
            for &sample in frame {
                let value = if sample.is_finite() {
                    sample.clamp(-1.0, 1.0)
                } else {
                    0.0
                };
                peak = peak.max(value.abs());
                let square = f64::from(value) * f64::from(value);
                frame_sum_squares += square;
                total_sum_squares += square;
            }
            let frame_rms = (frame_sum_squares / frame.len() as f64).sqrt() as f32;
            if frame_rms >= self.config.rms_threshold {
                voiced_samples += frame.len();
                trailing_silent_samples = 0;
                seen_voice = true;
            } else if seen_voice {
                trailing_silent_samples += frame.len();
            }
        }

        let voiced_duration = samples_to_duration(voiced_samples, sample_rate);
        (
            VadAnalysis {
                contains_voice: voiced_duration >= self.config.minimum_voice_duration,
                voiced_duration,
                trailing_silence: samples_to_duration(trailing_silent_samples, sample_rate),
            },
            LevelMeter {
                rms: (total_sum_squares / samples.len() as f64).sqrt() as f32,
                peak,
            },
        )
    }
}
