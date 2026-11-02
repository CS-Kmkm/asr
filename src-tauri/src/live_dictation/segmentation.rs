use crate::audio::PreparedAudio;
use std::time::Duration;

/// A frame at or above this fraction of the VAD threshold continues speech
/// (the hysteresis below the threshold that starts it).
pub(super) const VOICE_HYSTERESIS: f64 = 0.2;
/// Frame length of the endpoint analysis.
pub(super) const FRAME: Duration = Duration::from_millis(20);
/// Silence that ends a long utterance; shorter ones need `silence`.
const LONG_UTTERANCE_SILENCE: Duration = Duration::from_millis(350);
/// Speech at least this long is a long utterance.
const LONG_UTTERANCE: Duration = Duration::from_secs(12);

pub(super) enum Decision {
    Skip(Duration),
    Wait,
    Decode {
        samples: usize,
        endpoint: bool,
        voiced_until: Duration,
    },
}

pub(super) struct Segmentation {
    pub(super) silence: Duration,
    decoded_until: Duration,
    voiced_until: Duration,
    inference_cost: Duration,
}

impl Default for Segmentation {
    fn default() -> Self {
        Self {
            silence: Duration::from_millis(650),
            decoded_until: Duration::ZERO,
            voiced_until: Duration::ZERO,
            inference_cost: Duration::ZERO,
        }
    }
}

impl Segmentation {
    pub(super) fn plan(&self, audio: &PreparedAudio) -> Decision {
        let rate = audio.config.target_sample_rate as usize;
        let frame = frame_samples(rate);
        let padding = rate / 5;
        let minimum_voice =
            (audio.config.vad.minimum_voice_duration.as_secs_f64() * rate as f64) as usize;
        let mut first_voice = None;
        let mut last_voice = 0;
        let mut voiced = 0;
        for (index, values) in audio.samples.chunks(frame).enumerate() {
            let end = index * frame + values.len();
            let energy =
                values.iter().map(|v| f64::from(*v).powi(2)).sum::<f64>() / values.len() as f64;
            let rms = energy.sqrt();
            let voice_threshold = f64::from(audio.config.vad.rms_threshold);
            // Hysteresis: require normal voice energy to start recognition,
            // but a quieter continuation must not be treated as silence.
            if rms >= voice_threshold * VOICE_HYSTERESIS {
                if first_voice.is_none() && self.decoded_until.is_zero() && index * frame > padding
                {
                    return Decision::Skip(duration(index * frame - padding, rate));
                }
                first_voice.get_or_insert(index * frame);
                last_voice = end;
                if rms >= voice_threshold {
                    voiced += values.len();
                }
            } else if voiced >= minimum_voice {
                let speech_length = last_voice.saturating_sub(first_voice.unwrap_or(0));
                // A long utterance may end on a shorter breath, but time alone
                // never authorizes cutting through speech.
                let silence = if speech_length >= rate * LONG_UTTERANCE.as_secs() as usize {
                    self.silence.min(LONG_UTTERANCE_SILENCE)
                } else {
                    self.silence
                };
                let needed = (silence.as_secs_f64() * rate as f64) as usize;
                if end - last_voice >= needed {
                    return Decision::Decode {
                        samples: (last_voice + padding).min(end),
                        endpoint: true,
                        voiced_until: duration(last_voice, rate),
                    };
                }
            }
        }
        if self.decoded_until.is_zero()
            && voiced < minimum_voice
            && last_voice > 0
            && duration(audio.samples.len() - last_voice, rate) >= self.silence
        {
            return Decision::Skip(duration(audio.samples.len().saturating_sub(padding), rate));
        }
        // Keep enough leading silence for the next onset. Never re-copy a
        // completed utterance merely to discover that the microphone is quiet.
        if self.decoded_until.is_zero() {
            let leading = first_voice.unwrap_or(audio.samples.len());
            if leading > padding {
                return Decision::Skip(duration(leading - padding, rate));
            }
        }
        if voiced < minimum_voice {
            return Decision::Wait;
        }
        let latest_voice = duration(last_voice, rate);
        if latest_voice <= self.voiced_until {
            return Decision::Wait;
        }
        if audio.duration().saturating_sub(self.decoded_until)
            < self.required_new_audio(audio.duration())
        {
            return Decision::Wait;
        }
        Decision::Decode {
            samples: audio.samples.len(),
            endpoint: false,
            voiced_until: latest_voice,
        }
    }

    /// New audio a window of `window` needs before a decode that does not
    /// end the utterance. Requires useful new context; slow inference also
    /// gets breathing room. Continuous speech retains context instead of
    /// imposing an unsafe cut.
    fn required_new_audio(&self, window: Duration) -> Duration {
        let minimum_context: f64 = if self.decoded_until.is_zero() {
            1.2
        } else {
            1.5
        };
        Duration::from_secs_f64(
            minimum_context
                .max(self.inference_cost.as_secs_f64() * 2.5)
                .max(window.as_secs_f64() * 0.08),
        )
    }

    /// Whether a window of about `window` (within `tolerance`) could already
    /// hold enough new audio for a decode that does not end the utterance.
    pub(super) fn may_decode(&self, window: Duration, tolerance: Duration) -> bool {
        window.saturating_sub(self.decoded_until) + tolerance >= self.required_new_audio(window)
    }

    /// The shortest trailing silence that can end an utterance in a window
    /// of `window`; speech shorter than the window cannot be a long one.
    pub(super) fn shortest_endpoint_silence(&self, window: Duration) -> Duration {
        if window >= LONG_UTTERANCE {
            self.silence.min(LONG_UTTERANCE_SILENCE)
        } else {
            self.silence
        }
    }

    pub(super) fn recognized(
        &mut self,
        audio: Duration,
        voice: Duration,
        cost: Duration,
        endpoint: bool,
    ) {
        self.inference_cost = cost;
        if endpoint {
            self.decoded_until = Duration::ZERO;
            self.voiced_until = Duration::ZERO;
        } else {
            self.decoded_until = audio;
            self.voiced_until = voice;
        }
    }
}

fn frame_samples(rate: usize) -> usize {
    (rate * FRAME.as_millis() as usize / 1000).max(1)
}

/// Audio after the end of the last frame that `plan` counts as voice, or the
/// whole window when none is: the silence an endpoint decision measures.
pub(super) fn trailing_quiet(audio: &PreparedAudio) -> Duration {
    let rate = audio.config.target_sample_rate as usize;
    let frame = frame_samples(rate);
    let threshold = f64::from(audio.config.vad.rms_threshold) * VOICE_HYSTERESIS;
    let length = audio.samples.len();
    let voiced_end = (0..length.div_ceil(frame)).rev().find_map(|index| {
        let start = index * frame;
        let end = (start + frame).min(length);
        let values = &audio.samples[start..end];
        let energy =
            values.iter().map(|v| f64::from(*v).powi(2)).sum::<f64>() / values.len() as f64;
        (energy.sqrt() >= threshold).then_some(end)
    });
    duration(length - voiced_end.unwrap_or(0), rate)
}

fn duration(samples: usize, rate: usize) -> Duration {
    Duration::from_secs_f64(samples as f64 / rate as f64)
}

pub(super) fn append_utterance(prefix: &str, utterance: &str) -> String {
    let utterance = utterance.trim();
    if prefix.is_empty() {
        return utterance.to_owned();
    }
    if utterance.is_empty() {
        return prefix.to_owned();
    }
    let unspaced = |c: char| matches!(c as u32, 0x3000..=0x30FF | 0x3400..=0x9FFF | 0xF900..=0xFAFF | 0xFF00..=0xFFEF);
    let separator = if prefix.ends_with(char::is_whitespace)
        || prefix.chars().next_back().is_some_and(unspaced)
        || utterance.chars().next().is_some_and(unspaced)
    {
        ""
    } else {
        " "
    };
    format!("{prefix}{separator}{utterance}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::audio::CaptureConfig;

    fn audio(parts: &[(usize, bool)]) -> PreparedAudio {
        let samples = parts
            .iter()
            .flat_map(|(ms, speech)| vec![if *speech { 0.1 } else { 0.0 }; ms * 24])
            .collect();
        PreparedAudio {
            samples,
            config: CaptureConfig::default(),
        }
    }

    #[test]
    fn trailing_quiet_is_the_silence_an_endpoint_measures() {
        let ms = Duration::from_millis;
        assert_eq!(
            trailing_quiet(&audio(&[(200, false), (1000, true), (300, false)])),
            ms(300)
        );
        assert_eq!(trailing_quiet(&audio(&[(500, true)])), Duration::ZERO);
        assert_eq!(trailing_quiet(&audio(&[(400, false)])), ms(400));
        // The same silence ends the utterance once it reaches the endpoint.
        let policy = Segmentation::default();
        let ended = audio(&[(200, false), (1000, true), (650, false)]);
        assert_eq!(
            trailing_quiet(&ended),
            policy.shortest_endpoint_silence(ms(1850))
        );
        assert!(matches!(
            policy.plan(&ended),
            Decision::Decode { endpoint: true, .. }
        ));
        assert_eq!(policy.shortest_endpoint_silence(ms(12_000)), ms(350));
    }

    #[test]
    fn endpoint_keeps_padding_and_does_not_cut_short_interword_silence() {
        let policy = Segmentation::default();
        let input = audio(&[
            (200, false),
            (1000, true),
            (250, false),
            (1000, true),
            (700, false),
            (500, true),
        ]);
        match policy.plan(&input) {
            Decision::Decode {
                samples, endpoint, ..
            } => {
                assert!(endpoint);
                assert_eq!(samples, 2660 * 24); // 20 ms VAD frames plus 200 ms trailing padding.
            }
            _ => panic!("expected one complete utterance"),
        }
    }

    #[test]
    fn long_continuous_speech_is_never_cut_and_slow_inference_reduces_updates() {
        let mut policy = Segmentation::default();
        assert!(matches!(
            policy.plan(&audio(&[(20_000, true)])),
            Decision::Decode {
                endpoint: false,
                ..
            }
        ));
        policy.recognized(
            Duration::from_secs(20),
            Duration::from_secs(20),
            Duration::from_secs(4),
            false,
        );
        assert!(matches!(
            policy.plan(&audio(&[(24_000, true)])),
            Decision::Wait
        ));
        assert!(matches!(
            policy.plan(&audio(&[(30_000, true)])),
            Decision::Decode {
                endpoint: false,
                ..
            }
        ));
        assert!(matches!(
            policy.plan(&audio(&[(25_000, true), (400, false)])),
            Decision::Decode { endpoint: true, .. }
        ));
    }

    #[test]
    fn silence_is_skipped_without_discarding_next_onset() {
        let policy = Segmentation::default();
        assert!(
            matches!(policy.plan(&audio(&[(5000, false)])), Decision::Skip(d) if d == Duration::from_millis(4800))
        );
        assert!(
            matches!(policy.plan(&audio(&[(1000, false), (300, true)])), Decision::Skip(d) if d == Duration::from_millis(800))
        );
    }

    #[test]
    fn joining_utterances_preserves_repetitions_and_language_spacing() {
        assert_eq!(append_utterance("はい。", "はい。"), "はい。はい。");
        assert_eq!(append_utterance("2026", "年です。"), "2026年です。");
        assert_eq!(append_utterance("Go", "go now."), "Go go now.");
    }

    #[test]
    fn quiet_speech_uses_the_same_gain_as_recognition_before_endpoint_detection() {
        let mut samples: Vec<f32> = (0..28_800)
            .map(|i| (i as f32 * 0.1).sin() * 0.009)
            .collect();
        samples.extend(vec![0.0; 19_200]);
        let prepared = crate::audio::AudioSnapshot::for_test(samples, CaptureConfig::default())
            .prepare()
            .unwrap();
        assert!(matches!(
            Segmentation::default().plan(&prepared),
            Decision::Decode { endpoint: true, .. }
        ));
    }

    #[test]
    fn loud_to_quiet_continuation_is_not_mistaken_for_an_endpoint() {
        let mut prepared = audio(&[(1200, true)]);
        prepared.samples.extend(vec![0.004; 1200 * 24]);
        prepared.samples.extend(vec![0.0; 700 * 24]);
        assert!(
            matches!(Segmentation::default().plan(&prepared), Decision::Decode { samples, endpoint: true, .. } if samples == 2600 * 24)
        );
    }

    #[test]
    fn expired_short_blip_does_not_keep_the_entire_silent_recording() {
        assert!(
            matches!(Segmentation::default().plan(&audio(&[(40, true), (2000, false)])), Decision::Skip(d) if d == Duration::from_millis(1840))
        );
        assert!(matches!(
            Segmentation::default().plan(&audio(&[(40, true), (300, false)])),
            Decision::Wait
        ));
    }
}
