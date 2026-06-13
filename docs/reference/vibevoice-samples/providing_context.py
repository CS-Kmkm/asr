from transformers import AutoProcessor, VibeVoiceAsrForConditionalGeneration

model_id = "microsoft/VibeVoice-ASR-HF"
processor = AutoProcessor.from_pretrained(model_id)
model = VibeVoiceAsrForConditionalGeneration.from_pretrained(model_id, device_map="auto")
print(f"Model loaded on {model.device} with dtype {model.dtype}")

# Without context
inputs = processor.apply_transcription_request(
    audio="https://huggingface.co/datasets/bezzam/vibevoice_samples/resolve/main/realtime_model/vibevoice_tts_german.wav",
).to(model.device, model.dtype)
output_ids = model.generate(**inputs)
generated_ids = output_ids[:, inputs["input_ids"].shape[1] :]
transcription = processor.decode(generated_ids, return_format="transcription_only")[0]
print(f"WITHOUT CONTEXT: {transcription}")

# With context
inputs = processor.apply_transcription_request(
    audio="https://huggingface.co/datasets/bezzam/vibevoice_samples/resolve/main/realtime_model/vibevoice_tts_german.wav",
    prompt="About VibeVoice",
).to(model.device, model.dtype)
output_ids = model.generate(**inputs)
generated_ids = output_ids[:, inputs["input_ids"].shape[1] :]
transcription = processor.decode(generated_ids, return_format="transcription_only")[0]
print(f"WITH CONTEXT   : {transcription}")

"""
WITHOUT CONTEXT: Revevoices is a novel framework designed for generating expressive, long-form, multi-speaker conversational audio.
WITH CONTEXT   : VibeVoice is this novel framework designed for generating expressive, long-form, multi-speaker, conversational audio.
"""
