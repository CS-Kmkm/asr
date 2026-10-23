//! Deterministic post-capture signal processing.
use super::{AudioEnhancementConfig, LevelMeter, NoiseSuppressionLevel};

/// Applies inexpensive, deterministic post-capture processing. Keeping this out
/// of the CPAL callback prevents audio cleanup from competing with foreground
/// typing while the microphone is active.
pub fn enhance_audio(samples: &mut [f32], sample_rate: u32, config: AudioEnhancementConfig) {
    if samples.is_empty() || sample_rate == 0 {
        return;
    }

    sanitize_and_high_pass(samples, sample_rate);
    suppress_stationary_noise(samples, sample_rate, config.noise_suppression);

    let measured_rms = meter(samples).rms;
    let automatic_gain = if config.automatic_gain && measured_rms > 0.000_1 {
        // -20 dBFS is a conservative speech target. The cap prevents quiet
        // recordings from turning residual background noise into loud audio.
        (0.1 / measured_rms).clamp(0.5, 4.0)
    } else {
        1.0
    };
    let gain = config.gain * automatic_gain;
    for sample in samples {
        // A smooth limiter avoids hard clipping while preserving small signals.
        *sample = soft_limit(*sample * gain);
    }
}

fn sanitize_and_high_pass(samples: &mut [f32], sample_rate: u32) {
    let cutoff_hz = 70.0f32;
    let dt = 1.0 / sample_rate as f32;
    let rc = 1.0 / (2.0 * std::f32::consts::PI * cutoff_hz);
    let alpha = rc / (rc + dt);
    let mut previous_input = 0.0f32;
    let mut previous_output = 0.0f32;
    for sample in samples {
        let input = if sample.is_finite() { *sample } else { 0.0 };
        let output = alpha * (previous_output + input - previous_input);
        previous_input = input;
        previous_output = output;
        *sample = output;
    }
}

fn suppress_stationary_noise(samples: &mut [f32], sample_rate: u32, level: NoiseSuppressionLevel) {
    let strength = match level {
        NoiseSuppressionLevel::Off => return,
        NoiseSuppressionLevel::Low => 1.5,
        NoiseSuppressionLevel::Medium => 2.0,
        NoiseSuppressionLevel::High => 2.8,
    };
    let frame_len = (sample_rate as usize / 100).max(1); // 10 ms
    let mut frame_levels: Vec<f32> = samples
        .chunks(frame_len)
        .map(|frame| meter(frame).rms)
        .collect();
    if frame_levels.is_empty() {
        return;
    }
    frame_levels.sort_by(f32::total_cmp);
    let noise_floor = frame_levels[frame_levels.len() / 4].max(0.000_5);
    let threshold = noise_floor * strength;
    let minimum_gain = match level {
        NoiseSuppressionLevel::Low => 0.45,
        NoiseSuppressionLevel::Medium => 0.2,
        NoiseSuppressionLevel::High => 0.08,
        NoiseSuppressionLevel::Off => 1.0,
    };
    let mut smoothed_gain = 1.0f32;
    for frame in samples.chunks_mut(frame_len) {
        let rms = meter(frame).rms;
        let target_gain = if rms >= threshold {
            1.0
        } else {
            (rms / threshold).clamp(minimum_gain, 1.0)
        };
        // Open quickly for speech, close gradually to avoid audible pumping.
        let smoothing = if target_gain > smoothed_gain {
            0.65
        } else {
            0.18
        };
        smoothed_gain += (target_gain - smoothed_gain) * smoothing;
        for sample in frame {
            *sample *= smoothed_gain;
        }
    }
}

fn soft_limit(value: f32) -> f32 {
    if value.abs() <= 0.95 {
        value
    } else {
        value.signum() * (0.95 + 0.05 * ((value.abs() - 0.95) / 0.05).tanh())
    }
}

/// Averages complete interleaved frames into normalized mono samples.
pub fn interleaved_to_mono(samples: &[f32], channels: u16) -> Vec<f32> {
    let channels = channels as usize;
    if channels == 0 {
        return Vec::new();
    }
    samples
        .chunks_exact(channels)
        .map(|frame| frame.iter().copied().sum::<f32>() / channels as f32)
        .collect()
}

/// Deterministic linear interpolation resampler suitable for ASR preprocessing.
pub fn resample_linear(samples: &[f32], source_rate: u32, target_rate: u32) -> Vec<f32> {
    if samples.is_empty() || source_rate == 0 || target_rate == 0 {
        return Vec::new();
    }
    if source_rate == target_rate {
        return samples.to_vec();
    }

    let output_len = ((samples.len() as u64 * target_rate as u64) / source_rate as u64) as usize;
    if output_len == 0 {
        return Vec::new();
    }

    let scale = source_rate as f64 / target_rate as f64;
    (0..output_len)
        .map(|index| {
            let position = index as f64 * scale;
            let left = position.floor() as usize;
            let right = (left + 1).min(samples.len() - 1);
            let fraction = (position - left as f64) as f32;
            samples[left] + (samples[right] - samples[left]) * fraction
        })
        .collect()
}

/// Owned variant used by the recording pipeline. Downsampling is performed in
/// place, which avoids holding both the full input and output recordings at once.
pub(super) fn resample_linear_owned(
    mut samples: Vec<f32>,
    source_rate: u32,
    target_rate: u32,
) -> Vec<f32> {
    if samples.is_empty() || source_rate == 0 || target_rate == 0 {
        samples.clear();
        return samples;
    }
    if source_rate == target_rate {
        return samples;
    }
    if target_rate > source_rate {
        return resample_linear(&samples, source_rate, target_rate);
    }

    let output_len = ((samples.len() as u64 * target_rate as u64) / source_rate as u64) as usize;
    let scale = source_rate as f64 / target_rate as f64;
    for index in 0..output_len {
        let position = index as f64 * scale;
        let left = position.floor() as usize;
        let right = (left + 1).min(samples.len() - 1);
        let fraction = (position - left as f64) as f32;
        samples[index] = samples[left] + (samples[right] - samples[left]) * fraction;
    }
    samples.truncate(output_len);
    samples
}

pub fn meter(samples: &[f32]) -> LevelMeter {
    if samples.is_empty() {
        return LevelMeter::default();
    }
    let mut peak = 0.0f32;
    let mut sum_squares = 0.0f64;
    for sample in samples {
        let value = if sample.is_finite() {
            sample.clamp(-1.0, 1.0)
        } else {
            0.0
        };
        peak = peak.max(value.abs());
        sum_squares += f64::from(value) * f64::from(value);
    }
    LevelMeter {
        rms: (sum_squares / samples.len() as f64).sqrt() as f32,
        peak,
    }
}
