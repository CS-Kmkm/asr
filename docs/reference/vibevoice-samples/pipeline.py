from transformers import pipeline

model_id = "microsoft/VibeVoice-ASR-HF"
pipe = pipeline("any-to-any", model=model_id, device_map="auto")
chat_template = [
    {
        "role": "user",
        "content": [
            {"type": "text", "text": "About VibeVoice"},
            {
                "type": "audio",
                "path": "https://huggingface.co/datasets/bezzam/vibevoice_samples/resolve/main/realtime_model/vibevoice_tts_german.wav",
            },
        ],
    }
]
outputs = pipe(text=chat_template, return_full_text=False)

print("\n" + "=" * 60)
print("RAW PIPELINE OUTPUT")
print("=" * 60)
print(outputs)

"""
============================================================
RAW PIPELINE OUTPUT
============================================================
[{'input_text': [{'role': 'user', 'content': [{'type': 'text', 'text': 'About VibeVoice'}, {'type': 'audio', 'path': 'https://huggingface.co/datasets/bezzam/vibevoice_samples/resolve/main/realtime_model/vibevoice_tts_german.wav'}]}], 'generated_text': 'assistant\n[{"Start":0.0,"End":7.56,"Speaker":0,"Content":"VibeVoice is this novel framework designed for generating expressive, long-form, multi-speaker conversational audio."}]\n'}]
"""
