from transformers import AutoProcessor, VibeVoiceAsrForConditionalGeneration

model_id = "microsoft/VibeVoice-ASR-HF"
processor = AutoProcessor.from_pretrained(model_id)
model = VibeVoiceAsrForConditionalGeneration.from_pretrained(model_id, device_map="auto")
print(f"Model loaded on {model.device} with dtype {model.dtype}")

# Prepare inputs using `apply_transcription_request`
inputs = processor.apply_transcription_request(
    audio="https://huggingface.co/datasets/bezzam/vibevoice_samples/resolve/main/example_output/VibeVoice-1.5B_output.wav",
).to(model.device, model.dtype)

# Apply model
output_ids = model.generate(**inputs)
generated_ids = output_ids[:, inputs["input_ids"].shape[1] :]
transcription = processor.decode(generated_ids)[0]
print("\n" + "=" * 60)
print("RAW OUTPUT")
print("=" * 60)
print(transcription)

transcription = processor.decode(generated_ids, return_format="parsed")[0]
print("\n" + "=" * 60)
print("TRANSCRIPTION (list of dicts)")
print("=" * 60)
for speaker_transcription in transcription:
    print(speaker_transcription)

# Remove speaker labels, only get raw transcription
transcription = processor.decode(generated_ids, return_format="transcription_only")[0]
print("\n" + "=" * 60)
print("TRANSCRIPTION ONLY")
print("=" * 60)
print(transcription)

"""
============================================================
RAW OUTPUT
============================================================
<|im_start|>assistant
[{"Start":0,"End":15.43,"Speaker":0,"Content":"Hello everyone and welcome to the Vibe Voice podcast. I'm your host, Alex, and today we're getting into one of the biggest debates in all of sports: who's the greatest basketball player of all time? I'm so excited to have Sam here to talk about it with me."},{"Start":15.43,"End":21.05,"Speaker":1,"Content":"Thanks so much for having me, Alex. And you're absolutely right. This question always brings out some seriously strong feelings."},{"Start":21.05,"End":31.66,"Speaker":0,"Content":"Okay, so let's get right into it. For me, it has to be Michael Jordan. Six trips to the finals, six championships. That kind of perfection is just incredible."},{"Start":31.66,"End":40.93,"Speaker":1,"Content":"Oh man, the first thing that always pops into my head is that shot against the Cleveland Cavaliers back in '89. Jordan just rises, hangs in the air forever, and just sinks it."}]<|im_end|>
<|endoftext|>

============================================================
TRANSCRIPTION (list of dicts)
============================================================
{'Start': 0, 'End': 15.43, 'Speaker': 0, 'Content': "Hello everyone and welcome to the Vibe Voice podcast. I'm your host, Alex, and today we're getting into one of the biggest debates in all of sports: who's the greatest basketball player of all time? I'm so excited to have Sam here to talk about it with me."}
{'Start': 15.43, 'End': 21.05, 'Speaker': 1, 'Content': "Thanks so much for having me, Alex. And you're absolutely right. This question always brings out some seriously strong feelings."}
{'Start': 21.05, 'End': 31.66, 'Speaker': 0, 'Content': "Okay, so let's get right into it. For me, it has to be Michael Jordan. Six trips to the finals, six championships. That kind of perfection is just incredible."}
{'Start': 31.66, 'End': 40.93, 'Speaker': 1, 'Content': "Oh man, the first thing that always pops into my head is that shot against the Cleveland Cavaliers back in '89. Jordan just rises, hangs in the air forever, and just sinks it."}

============================================================
TRANSCRIPTION ONLY
============================================================
Hello everyone and welcome to the Vibe Voice podcast. I'm your host, Alex, and today we're getting into one of the biggest debates in all of sports: who's the greatest basketball player of all time? I'm so excited to have Sam here to talk about it with me. Thanks so much for having me, Alex. And you're absolutely right. This question always brings out some seriously strong feelings. Okay, so let's get right into it. For me, it has to be Michael Jordan. Six trips to the finals, six championships. That kind of perfection is just incredible. Oh man, the first thing that always pops into my head is that shot against the Cleveland Cavaliers back in '89. Jordan just rises, hangs in the air forever, and just sinks it.
"""
