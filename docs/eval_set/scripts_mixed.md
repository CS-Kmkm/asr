# 日英混在読み上げ台本

本台本は ASR 評価セット固定台本（日英混在）である。
**一度固定した台本は変更禁止。** 変更すると過去計測との比較が不能になる。
台本 ID とファイル命名規則は `README.md` を参照。

---

## 読み方の規約

- 日本語部分は日本語で、英語部分は英語で発話する。言語を切り替えずにどちらか一方に統一しないこと（この切り替え保持率が評価対象である）。
- 英語の製品名・コマンド名・ファイルパスは英語発音で読み上げる（例: `faster-whisper` は「ファスターウィスパー」ではなく「faster whisper」と英語で発音する）。
- 数字は前後の文脈の言語に合わせる（日本語文中なら日本語読み、英語文中なら英語読み）。
- 句読点は発話しない。
- 台本ごとに **想定秒数** を記す。

---

## 短文セット（約3秒 × 5本）

### mix_short_01  （想定: 約3秒）

```text
VibeVoiceでのrecognitionは非常に速いです
```

---

### mix_short_02  （想定: 約3秒）

```text
bench_audioフォルダにen_short_01.wavを置いてください
```

---

### mix_short_03  （想定: 約3秒）

```text
CUDAが利用できない場合はCPUモードにフォールバックします
```

---

### mix_short_04  （想定: 約3秒）

```text
田中エンジニアがfaster-whisperのbenchmarkを実行しました
```

---

### mix_short_05  （想定: 約3秒）

```text
WebSocketで接続してasr_workerにリクエストを送ります
```

---

## 中文セット（約15秒 × 3本）

### mix_mid_01  （想定: 約15秒）

```text
アカリテック株式会社のプロジェクトでは、VibeVoiceとfaster-whisperの両方をevaluateしました。CERとWERを計算するために、score_asr.pyスクリプトを使用しています。NFKC normalizationを適用した後の結果が最も安定していました。
```

---

### mix_mid_02  （想定: 約15秒）

```text
鈴木プロダクトマネージャーのreviewによると、pyprojectの依存関係にCTranslate2を追加する必要があります。nvidia-smiで確認したところ、十二ギガバイトのVRAMをほぼ使い切っていました。kotoba-whisperはJapanese-specificなタスクで優れたperformanceを示しています。
```

---

### mix_mid_03  （想定: 約15秒）

```text
bench_audioディレクトリには、ja_long_01.wavとen_long_01.wavおよびmix_long_01.wavの三種類のaudioファイルを配置してください。asr_workerプロセスはJSON Linesプロトコルでbenchmarkハーネスとcommunicateします。ReazonSpeechはJapanese corpusに特化したモデルであるため、混在発話のtestには注意が必要です。
```

---

## 長文（約60秒 × 1本）

### mix_long_01  （想定: 約60秒）

```text
本日のASR evaluationセッションを開始します。VibeVoiceとfaster-whisperの両バックエンドについて、日英混在発話の認識精度をmeasureします。田中エンジニアが用意したbench_audioフォルダには、二十四キロヘルツのmonoralなWAVファイルが格納されています。nvidia-smiコマンドでCUDAのVRAM使用量をmonitorしながら、asr_workerプロセスを起動してください。鈴木プロダクトマネージャーの要件では、warm状態での三秒発話についてゼロポイントファイブ秒以内のlatencyを目標としています。アカリテック株式会社向けのdocumentationには、pyprojectへのCTranslate2の追加手順とWebSocketの設定方法が記載されています。スコアリングにはNFKC normalizationを適用し、CERとWERの両metricsを算出します。kotoba-whisperとReazonSpeechは日本語に特化しているため、English utteranceの認識率が低下する可能性があります。十二ギガバイトのVRAMが確保できない環境では、CPU int8モードのfaster-whisperをfallbackとして使用します。term recallの計算では、terms.txtに記載されたすべての固有名詞がreferences.jsonのいずれかの正解文に含まれていることを事前にverifyしてください。
```

---

*台本固定日: 2026-06-13。以降の変更は CHANGELOG に記録し、旧バージョンの評価データと混在させないこと。*
