from transformers import AutoProcessor, VibeVoiceAsrForConditionalGeneration

model_id = "microsoft/VibeVoice-ASR-HF"
processor = AutoProcessor.from_pretrained(model_id)
model = VibeVoiceAsrForConditionalGeneration.from_pretrained(model_id, device_map="auto")
model.train()

# Prepare batch of 2
# -- NOTE: the original model is trained to output transcription, speaker ID, and timestamps in JSON-like format. Below we are only using the transcription text as the label
chat_template = [
    [
        {
            "role": "user",
            "content": [
                {"type": "text", "text": "VibeVoice is this novel framework designed for generating expressive, long-form, multi-speaker, conversational audio."},
                {
                    "type": "audio",
                    "path": "https://huggingface.co/datasets/bezzam/vibevoice_samples/resolve/main/realtime_model/vibevoice_tts_german.wav",
                },
            ],
        }
    ],
    [
        {
            "role": "user",
            "content": [
                {"type": "text", "text": "Hello everyone and welcome to the VibeVoice podcast. I'm your host, Alex, and today we're getting into one of the biggest debates in all of sports: who's the greatest basketball player of all time? I'm so excited to have Sam here to talk about it with me. Thanks so much for having me, Alex. And you're absolutely right. This question always brings out some seriously strong feelings. Okay, so let's get right into it. For me, it has to be Michael Jordan. Six trips to the finals, six championships. That kind of perfection is just incredible. Oh man, the first thing that always pops into my head is that shot against the Cleveland Cavaliers back in '89. Jordan just rises, hangs in the air forever, and just sinks it."},
                {
                    "type": "audio",
                    "path": "https://huggingface.co/datasets/bezzam/vibevoice_samples/resolve/main/example_output/VibeVoice-1.5B_output.wav",
                },
            ],
        }
    ],
]
inputs = processor.apply_chat_template(
    chat_template,
    tokenize=True,
    return_dict=True,
    output_labels=True,
).to(model.device, model.dtype)

loss = model(**inputs).loss
print("Loss:", loss.item())
loss.backward()
