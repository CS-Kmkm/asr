import type { Settings } from "./types";

export const executionPresets = ["fast_japanese", "live_streaming", "smart_local", "hybrid", "cloud_quality", "technical"] as const;
export type ExecutionPreset = typeof executionPresets[number];

/** Presets are one atomic settings patch, never an authorization to send data.
 * The user's endpoints, keys, consent, dictionary and style remain editable. */
export function executionPresetPatch(preset: ExecutionPreset): Partial<Settings> {
  const localSpeech: Partial<Settings> = {
    asrBackend: "faster-whisper", modelId: null, speechLocale: "ja-JP",
    statefulStreaming: false, incrementalFinalAsr: false, automaticVoiceMode: false,
    qwenDemoStreaming: false,
    liveTargetInsertion: false, speculativeCorrection: false,
  };
  switch (preset) {
    case "fast_japanese": return { ...localSpeech, asrBackend: "speech-cpp", modelId: "haas-ja-v1.0", textCorrectionEnabled: false };
    case "live_streaming": return { ...localSpeech, asrBackend: "sherpa-onnx", modelId: "nemotron-3.5-streaming-560ms", statefulStreaming: true, textCorrectionEnabled: false };
    case "smart_local": return { ...localSpeech, correctionProvider: "local", textCorrectionEnabled: true };
    case "hybrid": return { ...localSpeech, correctionProvider: "openai", textCorrectionEnabled: true };
    case "cloud_quality": return { ...localSpeech, asrBackend: "openai-compatible", correctionProvider: "openai", textCorrectionEnabled: true };
    case "technical": return { ...localSpeech, correctionProvider: "local", textCorrectionEnabled: true, personalizationEnabled: true, correctionMode: "conservative" };
  }
}
