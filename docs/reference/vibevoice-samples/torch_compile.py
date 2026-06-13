import time
import torch
from transformers import AutoProcessor, VibeVoiceAsrForConditionalGeneration

model_id = "microsoft/VibeVoice-ASR-HF"

num_warmup = 5
num_runs = 20

# Load processor + model
processor = AutoProcessor.from_pretrained(model_id)
model = VibeVoiceAsrForConditionalGeneration.from_pretrained(model_id, torch_dtype=torch.bfloat16,).to("cuda")

# Prepare static inputs
chat_template = [
    [
        {
            "role": "user",
            "content": [
                {
                    "type": "text",
                    "text": "VibeVoice is this novel framework designed for generating expressive, long-form, multi-speaker, conversational audio.",
                },
                {
                    "type": "audio",
                    "path": "https://huggingface.co/datasets/bezzam/vibevoice_samples/resolve/main/realtime_model/vibevoice_tts_german.wav",
                },
            ],
        }
    ],
] * 4  # batch size 4
inputs = processor.apply_chat_template(
    chat_template,
    tokenize=True,
    return_dict=True,
).to("cuda", torch.bfloat16)

# Benchmark without compile
print("Warming up without compile...")
with torch.no_grad():
    for _ in range(num_warmup):
        _ = model(**inputs)

torch.cuda.synchronize()

print("\nBenchmarking without torch.compile...")
torch.cuda.synchronize()
start = time.time()
with torch.no_grad():
    for _ in range(num_runs):
        _ = model(**inputs)
torch.cuda.synchronize()
no_compile_time = (time.time() - start) / num_runs
print(f"Average time without compile: {no_compile_time:.4f}s")

# Benchmark with compile
print("\nCompiling model...")
model = torch.compile(model)

print("Warming up with compile (includes graph capture)...")
with torch.no_grad():
    for _ in range(num_warmup):
        _ = model(**inputs)

torch.cuda.synchronize()

print("\nBenchmarking with torch.compile...")
torch.cuda.synchronize()
start = time.time()
with torch.no_grad():
    for _ in range(num_runs):
        _ = model(**inputs)
torch.cuda.synchronize()
compile_time = (time.time() - start) / num_runs
print(f"Average time with compile: {compile_time:.4f}s")

speedup = no_compile_time / compile_time
print(f"\nSpeedup: {speedup:.2f}x")
