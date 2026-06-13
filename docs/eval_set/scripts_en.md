# English Reading Script

This is the fixed reading script (English) for the ASR evaluation set.
**Once fixed, the script must not be modified.** Any change invalidates comparisons with past measurements.
See `README.md` for script IDs and file naming conventions.

---

## Pronunciation Guidelines

- Numbers are read as natural English (e.g. "12 GB" → "twelve gigabytes").
- File paths and command names are read as natural English words or letter-by-letter as noted in the reference text (references.json).
- Do not speak punctuation marks (they are stripped during scoring).
- Each script includes an **estimated duration** based on a typical English reading pace (~130 words/min).

---

## Short Sentences (approx. 3 seconds × 5 items)

### en_short_01  (estimated: ~3 seconds)

```text
Please schedule the meeting for two thirty PM on Friday
```

---

### en_short_02  (estimated: ~3 seconds)

```text
The invoice total is three hundred and fifty dollars
```

---

### en_short_03  (estimated: ~3 seconds)

```text
Send the report to the product manager by end of day
```

---

### en_short_04  (estimated: ~3 seconds)

```text
The faster-whisper model requires twelve gigabytes of VRAM
```

---

### en_short_05  (estimated: ~3 seconds)

```text
VibeVoice recognized the utterance in zero point five seconds
```

---

## Medium Sentences (approx. 15 seconds × 3 items)

### en_mid_01  (estimated: ~15 seconds)

```text
According to the benchmark results, the warm latency for a three-second utterance was well below eight hundred milliseconds. The faster-whisper large model running on CUDA achieved the lowest character error rate among all tested backends. We will proceed with this configuration for daily input mode.
```

---

### en_mid_02  (estimated: ~15 seconds)

```text
Please place the evaluation audio files in the bench_audio directory. Each file should be named using the language code, category, and sequence number, for example en_short_01 dot wav. The pyproject file lists CTranslate2 as a required dependency for running the asr_worker process.
```

---

### en_mid_03  (estimated: ~15 seconds)

```text
ReazonSpeech and kotoba-whisper are specialized models for Japanese, but they may underperform on English utterances. For mixed-language inputs, the VibeVoice backend showed strong results in our preliminary tests. The NFKC normalization step is applied before computing both CER and WER scores.
```

---

## Long Passage (approx. 60 seconds × 1 item)

### en_long_01  (estimated: ~60 seconds)

```text
Welcome to the VibeVoice ASR evaluation session. In this test we will measure the recognition accuracy and latency across multiple audio conditions. All recordings should be saved as twenty-four kilohertz mono sixteen-bit WAV files in the bench_audio folder. The nvidia-smi command can be used to monitor VRAM usage during the transcription process. Our target is to complete transcription of a three-second utterance within eight hundred milliseconds in the warm state, which is defined as the model already loaded into GPU memory. The asr_worker process communicates with the benchmark harness over a JSON Lines protocol via standard input and output. The desktop application shell is built with Tauri, which bundles the frontend and the Rust backend into a single executable. For the accuracy evaluation, we use the score_asr dot py script, which reads a reference map from references dot json and computes character error rate, word error rate, and term recall. The term list in terms dot txt includes proper nouns such as VibeVoice, faster-whisper, CTranslate2, and CUDA that are intentionally embedded in the reference sentences. The pyproject configuration file specifies all required dependencies. Results are stored in the results directory and summarized in the benchmarks document.
```

---

*Script fixed on: 2026-06-13. Any future changes must be logged in CHANGELOG; do not mix old and new evaluation data.*
