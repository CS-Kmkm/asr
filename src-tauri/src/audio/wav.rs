//! WAV encoding and unique temporary artifact names.
use super::AudioError;
use std::{
    fs::File,
    io::{BufWriter, Write},
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

pub(super) fn write_pcm16_wav(
    path: &Path,
    samples: &[f32],
    sample_rate: u32,
) -> Result<(), AudioError> {
    let data_size = samples
        .len()
        .checked_mul(2)
        .and_then(|size| u32::try_from(size).ok())
        .ok_or(AudioError::InvalidConfiguration(
            "WAV artifact is too large",
        ))?;
    let riff_size = 36u32
        .checked_add(data_size)
        .ok_or(AudioError::InvalidConfiguration(
            "WAV artifact is too large",
        ))?;
    let mut writer = BufWriter::new(File::create(path)?);

    writer.write_all(b"RIFF")?;
    writer.write_all(&riff_size.to_le_bytes())?;
    writer.write_all(b"WAVEfmt ")?;
    writer.write_all(&16u32.to_le_bytes())?;
    writer.write_all(&1u16.to_le_bytes())?;
    writer.write_all(&1u16.to_le_bytes())?;
    writer.write_all(&sample_rate.to_le_bytes())?;
    writer.write_all(&(sample_rate * 2).to_le_bytes())?;
    writer.write_all(&2u16.to_le_bytes())?;
    writer.write_all(&16u16.to_le_bytes())?;
    writer.write_all(b"data")?;
    writer.write_all(&data_size.to_le_bytes())?;
    const SAMPLES_PER_CHUNK: usize = 4096;
    let mut encoded = [0u8; SAMPLES_PER_CHUNK * 2];
    for chunk in samples.chunks(SAMPLES_PER_CHUNK) {
        for (sample, bytes) in chunk.iter().zip(encoded.chunks_exact_mut(2)) {
            let normalized = if sample.is_finite() {
                sample.clamp(-1.0, 1.0)
            } else {
                0.0
            };
            bytes.copy_from_slice(&((normalized * i16::MAX as f32).round() as i16).to_le_bytes());
        }
        writer.write_all(&encoded[..chunk.len() * 2])?;
    }
    writer.flush()?;
    Ok(())
}

pub(super) fn temporary_wav_path(directory: Option<&Path>) -> PathBuf {
    static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let nonce = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    directory
        .map(Path::to_path_buf)
        .unwrap_or_else(std::env::temp_dir)
        .join(format!(
            "local-ai-voice-{}-{timestamp}-{nonce}.wav",
            std::process::id()
        ))
}
