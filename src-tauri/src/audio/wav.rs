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
        for (sample, bytes) in chunk.iter().zip(encoded.as_chunks_mut::<2>().0.iter_mut()) {
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

/// Reads a mono 16-bit PCM WAV as written by [`write_pcm16_wav`]: its
/// samples and sample rate. Any other layout, or lengths that disagree with
/// the file, is an error.
pub(crate) fn read_pcm16_wav(path: &Path) -> Result<(Vec<f32>, u32), AudioError> {
    let bytes = std::fs::read(path)?;
    let field =
        |at: usize| u32::from_le_bytes([bytes[at], bytes[at + 1], bytes[at + 2], bytes[at + 3]]);
    let valid = bytes.len() >= 44 && {
        let sample_rate = field(24);
        let data_size = field(40) as usize;
        &bytes[0..4] == b"RIFF"
            && field(4) as usize == bytes.len() - 8
            && &bytes[8..16] == b"WAVEfmt "
            && field(16) == 16
            && bytes[20..24] == [1, 0, 1, 0]
            && sample_rate > 0
            && sample_rate.checked_mul(2) == Some(field(28))
            && bytes[32..36] == [2, 0, 16, 0]
            && &bytes[36..40] == b"data"
            && data_size == bytes.len() - 44
            && data_size.is_multiple_of(2)
    };
    if !valid {
        return Err(AudioError::InvalidConfiguration(
            "WAV artifact has an unexpected layout",
        ));
    }
    let sample_rate = field(24);
    let samples = bytes[44..]
        .as_chunks::<2>()
        .0
        .iter()
        .map(|pair| f32::from(i16::from_le_bytes(*pair)) / f32::from(i16::MAX))
        .collect();
    Ok((samples, sample_rate))
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_back_what_it_writes_and_rejects_altered_files() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("roundtrip.wav");
        let samples = [0.0, 0.5, -0.5, 1.0, -1.0];
        write_pcm16_wav(&path, &samples, 16_000).unwrap();
        let (read, rate) = read_pcm16_wav(&path).unwrap();
        assert_eq!(rate, 16_000);
        assert_eq!(read.len(), samples.len());
        for (read, written) in read.iter().zip(samples) {
            assert!((read - written).abs() < 1e-4, "{read} {written}");
        }

        let valid = std::fs::read(&path).unwrap();
        let truncated = &valid[..valid.len() - 2];
        let mut odd = valid.clone();
        odd.push(0);
        let mut stereo = valid.clone();
        stereo[22] = 2;
        let mut no_rate = valid.clone();
        no_rate[24..28].fill(0);
        for (name, bytes) in [
            ("truncated", truncated.to_vec()),
            ("odd", odd),
            ("stereo", stereo),
            ("no_rate", no_rate),
            ("header", valid[..20].to_vec()),
        ] {
            let altered = directory.path().join(format!("{name}.wav"));
            std::fs::write(&altered, bytes).unwrap();
            assert!(read_pcm16_wav(&altered).is_err(), "{name}");
        }
    }
}
