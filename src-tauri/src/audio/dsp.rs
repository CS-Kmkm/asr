//! Deterministic post-capture signal processing.
use super::{AudioEnhancementConfig, EnergyVadConfig, LevelMeter, NoiseSuppressionLevel};

/// Applies inexpensive, deterministic post-capture processing. Keeping this out
/// of the CPAL callback prevents audio cleanup from competing with foreground
/// typing while the microphone is active.
#[cfg(test)]
pub fn enhance_audio(samples: &mut [f32], sample_rate: u32, config: AudioEnhancementConfig) {
    enhance_audio_with_vad(samples, sample_rate, config, EnergyVadConfig::default());
}

pub(super) fn enhance_audio_with_vad(
    samples: &mut [f32],
    sample_rate: u32,
    config: AudioEnhancementConfig,
    vad: EnergyVadConfig,
) {
    if samples.is_empty() || sample_rate == 0 {
        return;
    }

    sanitize_and_high_pass(samples, sample_rate);
    suppress_stationary_noise(samples, sample_rate, config.noise_suppression, vad);

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

fn silent_noise_floor(samples: &[f32], sample_rate: u32, vad: EnergyVadConfig) -> Option<f32> {
    let frame_len = super::duration_to_samples(vad.frame_duration, sample_rate).max(1);
    let mut silent_levels: Vec<f32> = samples
        .chunks(frame_len)
        .map(|frame| meter(frame).rms)
        .filter(|rms| *rms < vad.rms_threshold)
        .collect();
    if silent_levels.is_empty() {
        return None;
    }
    silent_levels.sort_by(f32::total_cmp);
    Some(silent_levels[silent_levels.len() / 2].max(0.000_5))
}

fn suppress_stationary_noise(
    samples: &mut [f32],
    sample_rate: u32,
    level: NoiseSuppressionLevel,
    vad: EnergyVadConfig,
) {
    let strength = match level {
        NoiseSuppressionLevel::Off => return,
        NoiseSuppressionLevel::Low => 1.5,
        NoiseSuppressionLevel::Medium => 2.0,
        NoiseSuppressionLevel::High => 2.8,
    };
    let frame_len = (sample_rate as usize / 100).max(1); // 10 ms
                                                         // Dense speech is not a noise sample. Without a VAD-silent frame, leave
                                                         // the signal alone rather than estimating the floor from quiet phonemes.
    let Some(noise_floor) = silent_noise_floor(samples, sample_rate, vad) else {
        return;
    };
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
#[cfg(test)]
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

/// Linear interpolation preceded by an anti-alias filter when downsampling.
pub fn resample_linear(samples: &[f32], source_rate: u32, target_rate: u32) -> Vec<f32> {
    if samples.is_empty() || source_rate == 0 || target_rate == 0 {
        return Vec::new();
    }
    if source_rate == target_rate {
        return samples.to_vec();
    }
    if target_rate < source_rate {
        return resample_linear_owned(samples.to_vec(), source_rate, target_rate);
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

    anti_alias(&mut samples, source_rate, target_rate);

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

/// Eighth-order Butterworth low-pass, four biquads in f64. The cutoff leaves
/// a transition band before the output Nyquist frequency. Processing in place
/// keeps memory bounded for long recordings; state is reset for each take.
fn anti_alias(samples: &mut [f32], source_rate: u32, target_rate: u32) {
    let omega = std::f64::consts::PI * 0.8 * target_rate as f64 / source_rate as f64;
    let cosine = omega.cos();
    for stage in 0..4 {
        let q = 1.0 / (2.0 * (((2 * stage + 1) as f64 * std::f64::consts::PI) / 16.0).cos());
        let alpha = omega.sin() / (2.0 * q);
        let a0 = 1.0 + alpha;
        let b0 = (1.0 - cosine) / (2.0 * a0);
        let b1 = 2.0 * b0;
        let a1 = -2.0 * cosine / a0;
        let a2 = (1.0 - alpha) / a0;
        let (mut z1, mut z2) = (0.0, 0.0);
        for sample in samples.iter_mut() {
            let input = if sample.is_finite() {
                f64::from(*sample)
            } else {
                0.0
            };
            let output = b0 * input + z1;
            z1 = b1 * input - a1 * output + z2;
            z2 = b0 * input - a2 * output;
            *sample = output as f32;
        }
    }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resampler_preserves_speech_band_and_rejects_aliases() {
        for (source, target, rejected) in [
            (48_000, 24_000, 18_000.0),
            (48_000, 16_000, 14_000.0),
            (44_100, 16_000, 14_000.0),
        ] {
            let amplitude = |frequency: f64| {
                let signal: Vec<f32> = (0..source)
                    .map(|i| {
                        (2.0 * std::f64::consts::PI * frequency * i as f64 / source as f64).sin()
                            as f32
                            * 0.5
                    })
                    .collect();
                let output = resample_linear_owned(signal, source, target);
                assert_eq!(output.len(), target as usize);
                meter(&output[1_000..]).rms
            };
            let speech = amplitude(1_000.0);
            assert!(speech > 0.34 && speech < 0.36, "speech RMS {speech}");
            let alias = amplitude(rejected);
            assert!(
                alias < speech * 0.01,
                "alias RMS {alias} at {source}->{target}"
            );
        }
    }

    #[test]
    fn noise_floor_uses_only_vad_silent_frames() {
        let vad = EnergyVadConfig::default();
        let mut samples = vec![0.002; 480];
        samples.extend(vec![0.008; 480]);
        samples.extend(vec![0.2; 24_000]);
        assert!((silent_noise_floor(&samples, 24_000, vad).unwrap() - 0.008).abs() < 1e-6);
        assert_eq!(silent_noise_floor(&vec![0.2; 24_000], 24_000, vad), None);
        let mut dense = vec![0.02; 2_400];
        let before = dense.clone();
        suppress_stationary_noise(&mut dense, 24_000, NoiseSuppressionLevel::High, vad);
        assert_eq!(dense, before);
    }
}
