# ADR-0004: デフォルトASRバックエンドの選定 — faster-whisper

## ステータス

採用済み (2026-06-13)

## 文脈

本アプリのヘッドラインKPIは **3秒発話・warm状態で「停止→挿入完了」p50 < 1.5秒** である
(計画書 §6.1、benchmarks.md §1.1)。
このKPIを満たすには、ASR成分単体でwarm duration p50 < 800ms が必要になる
(benchmarks.md §4.1)。

### VibeVoice-ASR-HFの特性と制約

VibeVoice-ASR-HFはLLM型ASR(生成デコード)であり、長時間音声・話者情報・タイムスタンプ・
多言語混在といった特性に強みを持つ(計画書 §9.2)。
しかし、以下の制約により日常の短文入力をデフォルトとすることは適切でない。

- **遅延**: LLM型ゆえ短文であっても生成デコードコストが発生し、
  warm duration p50 < 800ms を満たせない可能性が高い(benchmarks.md §2「読み筋」)。
- **CUDA依存**: CPU実行が不可であり、NVIDIA GPU(VRAM 12GB以上)が必須。
- **重量依存スタック**: torch + torchvision + transformers + bitsandbytes + accelerate +
  librosa の合計が数GBに及び、ADR-0003が問題提起した配布サイズ問題を悪化させる。
- **実装の乖離**: コードがVibeVoiceをハードコードされたデフォルトとしており、
  計画書 §9「短文=低遅延ASR、長文=VibeVoice」および benchmarks.md §3 の戦略と
  矛盾していた。

### faster-whisperの特性

faster-whisper (CTranslate2バックエンド) は以下の特性を持つ。

- CPU (int8) またはCUDA (int8/fp16) で動作する。
- `large-v3-turbo` モデルはWhisper系の中で遅延・精度のバランスが優れている。
- `initial_prompt` によるホットワード/辞書連携が可能(計画書 §9.1)。
- 依存スタックが軽量で、GPU不要の環境でも動作する。

## 決定

**デフォルトASRバックエンドを faster-whisper (large-v3-turbo) とする。**

具体的には以下を決定する。

1. **デフォルトバックエンド**: `faster-whisper`。デフォルトモデル: `large-v3-turbo`。
   CPU・GPU 両対応。GPU がある場合はCUDAを自動使用し、ない場合はCPU int8 で動作する。
2. **VibeVoice はオプションGPUバックエンド**: 長文・高精度・議事録用途に限定し、
   Settings画面で選択可能なオプションとして維持する。
3. **依存関係の再編**: `pip install -e .` がデフォルトで faster-whisper スタック
   (CPU対応・軽量) をインストールする。VibeVoice用の重量スタック
   (torch, torchvision, transformers, bitsandbytes, accelerate, librosa) は
   `pip install -e ".[vibevoice]"` オプショナルエクストラとしてのみインストールされる。
4. **バックエンド識別のモデルID**: モデルのアイデンティティはバックエンド設定から
   導出され、コードにハードコードしない。

## 結果

### 採用によるメリット

- **軽量デフォルトインストール**: `pip install -e .` でGPU・torch不要の環境が整う。
  GPU非搭載のPCでもアプリが動作する(CPU品質は劣るが機能する)。
- **ADR-0003の緩和**: デフォルトインストールにtorch+CUDAが不要となり、
  配布サイズ問題(ADR-0003の主要課題)が大幅に軽減される。
- **KPI達成可能性の向上**: faster-whisper (large-v3-turbo, int8/fp16) は
  warm duration p50 < 800ms を満たす見込みが高い(benchmarks.md §4.1)。
- **計画との整合**: 計画書 §9 の「短文=低遅延ASR、長文=VibeVoice」戦略を
  実装レベルで実現する。

### トレードオフ・残存課題

- **VibeVoiceユーザーの追加手順**: VibeVoice目的のユーザーは
  `pip install -e ".[vibevoice]"` の実行と Settings での切り替えが必要になる。
- **CPU動作時の品質**: CPU int8 の `large-v3-turbo` は精度・速度ともGPU動作より
  劣る。benchmarks.md §4.1 の実測でKPIを確認すること。
- **「GPU必須」の前提解除**: 従来のGPU必須フレーミングを変更するため、
  ドキュメント全体の前提条件表記を更新が必要(本ADR適用によるドキュメント更新で対応)。

### VibeVoiceユーザーへの影響

```powershell
# VibeVoice バックエンドを使う場合
pip install -e ".[vibevoice]"
# 起動後、Settings → ASR backend で VibeVoice (GPU) を選択する
```

NVIDIA CUDA対応GPU(VRAM 12GB以上)と `nvidia-smi` が必要(変更なし)。

## 関連

- ADR-0001: 技術スタック選定(Pythonサイドカーの設計経緯)
- ADR-0003: Pythonサイドカーの配布方式(torch+CUDA多GBが配布上の主要課題)
- 計画書 §9: ASR戦略(短文=低遅延ASR、長文=VibeVoice の方針)
- benchmarks.md §3/§4.1: 計測プロトコルとデフォルト選定ロジック
