# ASR ベンチマークと Phase 0 ゲート判定

Haas／Parakeet JapaneseのWindows/Vulkan導入・Whisperとの固定200音声比較は、
[日本語モデル比較記録](japanese_model_comparison.md)を参照してください。既定モデルの選定ゲートは未通過です。

人が読み直さずに日本語ASRを評価する手順は、
[公開音声による評価](eval_set/public_datasets.md)を参照してください。
固定済みのFLEURS＋JVNV、batch採点、実時間streaming replayを利用できます。

OpenWhisprとの日本語エンジン比較の条件・再現手順・結果は
[競合比較](competitor_comparison.md)を参照。

本ドキュメントは、計画書 (`docs/local_ai_voice_input_implementation_plan.md`) の
**Phase 0(技術検証, §15)** が定める

> 「日常入力に十分な低遅延が出ない ASR は、長文・議事録・高精度モードに限定する。
> 日常入力には低遅延 ASR プロバイダーを採用する。」

というゲート判定の **正式な記録場所** である。計測プロトコル、候補比較、判定基準、
そして結果を記入する空欄テンプレートをここに集約する。実計測の生データ(JSON)は
`scripts/bench_asr.py --json` の出力を `results/` 等に保存し、要約だけを本ドキュメントの
テンプレート表に転記する運用とする。

> 実行環境の注意: 本リポジトリの開発環境(WSL2, GPU なし)では `mock` バックエンドで
> ハーネスの動作確認のみ可能。**実数値は Windows 実機(NVIDIA GPU 12-16GB)で計測する。**
> 後述の「Windows 実機での実行手順」を参照。

---

## 1. 目的と KPI 定義

### 1.1 ヘッドライン KPI(日常入力)

短文の日常入力が「速い」と体感できるかを一本の指標で判定する。

| KPI | 目標 |
|---|---|
| **3秒発話・warm 状態で「停止 → 挿入完了」** | **p50 < 1.5秒 / p95 < 2.5秒** |

`warm` = モデルロード済み・常駐状態。`cold`(初回ロード込み)は別途記録するが、
ヘッドライン判定には用いない(初回のみ発生し、常駐運用で隠蔽できるため)。

### 1.2 全体予算の分解(stop → insertion)

計画書 §6.1「計測の分解」に従い、停止後の全体時間を成分へ割り当てる。
下表は**3秒発話・warm のヘッドライン p50 < 1.5秒**を成立させるための内訳目安(予算)。
各成分は `bench_asr.py` および実装側の計測ログで個別に検証する。

| 成分 | 内容 | p50 予算 | p95 予算 |
|---|---|---:|---:|
| 音声前処理 | 録音終了 → リサンプル/正規化完了(VAD 終端検出含む) | < 150ms | < 250ms |
| **ASR 成分** | **ワーカー transcribe(純粋処理 `duration_ms`)** | **< 800ms** | **< 1500ms** |
| IPC/直列化 | UI ↔ ワーカー往復(`wall_ms − duration_ms`) | < 100ms | < 200ms |
| LLM 整形 | 日常入力デフォルトでは無効/最小整形(Phase 1)。有効時は別予算 | (0) | (0) |
| 保護スパン検査 | 短文・無整形時はほぼ 0 | < 50ms | < 100ms |
| 挿入処理 | UIA / Clipboard / SendInput | < 300ms | < 450ms |
| **合計** | | **< 1500ms** | **< 2500ms** |

> ASR 成分予算 **p50 < 800ms** がプロバイダー選定の主基準。
> `bench_asr.py` の「warm duration (ASR pure)」がこの成分に直接対応する。
> 「warm wall (round trip)」は ASR 成分 + IPC/直列化に対応する。

### 1.3 長文系 KPI(併記)

計画書 §6.1 / §15 の従来目標を長文系 KPI として併記する。日常入力 KPI を満たさない
プロバイダーでも、この長文系 KPI を満たせば「長文・議事録・高精度モード」担当として
採用しうる(ゲートの分岐先)。

| KPI | 目標 |
|---|---|
| 10秒発話・warm 状態で「停止 → 挿入完了」 | p50 < 3秒 / p95 < 6秒 |
| 12GB GPU・デフォルト構成 | OOM しない |
| 固有名詞(辞書あり) | 正解率が明確に改善する |

---

## 2. 候補モデル比較表(実測前の事前情報)

> 下表は**実測前の事前知識による見込み**であり、数値は当たりを付けるための目安。
> ライセンス・対応言語・VRAM はいずれも **実機/一次情報で要確認**。Web アクセスせず
> 知識の範囲で記載しているため、確定情報は実機検証時に上書きすること。

| モデル | アーキテクチャ | 日本語 | 英語 | 日英混在 | VRAM 目安 | CPU 実行 | ホットワード/プロンプト | ライセンス(要確認) |
|---|---|---|---|---|---|---|---|---|
| VibeVoice-ASR (4bit) | LLM 型 ASR(生成デコード) | 良(見込み) | 良 | 良(強み・見込み) | ~6-9GB(要確認) | 不可(CUDA 必須) | あり(prompt/ホットワード) | 要確認(MS 系) |
| VibeVoice-ASR (8bit) | 同上 | 良 | 良 | 良 | ~10-13GB(要確認) | 不可 | あり | 要確認 |
| VibeVoice-ASR (bf16) | 同上 | 良 | 良 | 良 | ~16GB+(要確認) | 不可 | あり | 要確認 |
| faster-whisper large-v3-turbo | Whisper(CTC/AED, CTranslate2) | 良 | 良 | 中-良 | ~3-6GB(int8/fp16, 要確認) | 可(int8, 遅い) | プロンプト(initial_prompt), 簡易ホットワード | MIT 系(要確認) |
| distil-large-v3 | Whisper 蒸留 | 中-良(英寄り, 要確認) | 良 | 中 | ~2-4GB(要確認) | 可(int8) | initial_prompt | MIT 系(要確認) |
| faster-whisper small (CPU int8) | Whisper small (CTranslate2) | 中 | 中-良 | 中 | CPU 実行(VRAM 不要) | **可(主用途)** | initial_prompt | MIT 系(要確認) |
| kotoba-whisper | Whisper 派生(日本語特化) | **優(日本語特化)** | 中 | 中(要確認) | ~2-4GB(要確認) | 可(派生実装次第) | initial_prompt | Apache/MIT 系(要確認) |
| ReazonSpeech | 日本語 ASR(NeMo/ESPnet 系, 要確認) | **優(日本語特化)** | 弱-不可 | 弱 | ~2-4GB(要確認) | 一部可(実装依存) | 実装依存(辞書/プロンプト要確認) | 要確認(コーパス由来制約に注意) |
| NVIDIA Parakeet 系 (TDT, 多言語) | Transducer(TDT/RNN-T, NeMo) | 中-良(多言語版・要確認) | 優 | 中-良(要確認) | ~3-6GB(要確認) | 限定的(NeMo, CPU は非推奨) | ホットワード/外部辞書(要確認) | 要確認(NVIDIA, モデル別ライセンス差) |

**読み筋(事前仮説)**

- 日常入力デフォルトは **低遅延が出る Whisper 系(faster-whisper turbo / distil)** か
  **日本語特化(kotoba-whisper)** が有力候補。warm の ASR 成分 p50 < 800ms を満たすかが鍵。
- **VibeVoice-ASR は精度・話者・タイムスタンプ・長文に強み**だが、LLM 型ゆえ短文遅延が
  読めない。短文で予算超過なら長文/高精度モード担当へ回す(ゲートの分岐先)。
- **GPU 制約/フォールバック用に faster-whisper small (CPU int8)** を必ず 1 つ確保。
- 日英混在は VibeVoice / Whisper 系が無難。日本語特化系は混在で劣化しうる(要計測)。

---

## 3. 計測プロトコル

### 3.1 計測マトリクス

| 軸 | 値 |
|---|---|
| 発話長 | 3秒 / 5秒 / 15秒 / 60秒 |
| 言語 | 日本語 / 英語 / 日英混在 |
| 状態 | cold(プロセス起動 + 初回ロード込み)/ warm(常駐・反復) |
| 反復 | warm は各条件 **5回以上**(推奨 10回)、cold は **3回以上**(プロセス再起動込み) |
| 量子化 | VibeVoice は 4bit / 8bit / bf16 を比較。Whisper 系は int8 / fp16 |

各条件(発話長 × 言語)について以下を収集する。

- **遅延**: `wall_ms`(往復)と `duration_ms`(ASR 純粋処理)を区別して p50 / p95 / min / max。
  - `wall_ms − duration_ms` ≈ IPC/直列化オーバーヘッド。
- **cold load**: プロセス起動 + `load` 完了までの時間(初回ロード時間, §9.3)。
- **VRAM**: 後述の方法で `load` 後と `transcribe` 中のピークを記録。
- **精度**: CER / WER / 固有名詞正解率(後述)。

### 3.2 遅延の測り方

`scripts/bench_asr.py` が JSONL プロトコル(`load → transcribe×N → shutdown`)を駆動し、
2 種類の時間を分離計測する。

- `duration_ms`: ワーカーが返す純 ASR 処理時間(`worker.py` が `transcribe` 内で計測)。
- `wall_ms`: ハーネスが測るリクエスト往復時間(JSON 直列化 + パイプ + 計算)。

cold は `--cold-runs` でプロセスを毎回再起動して測定(load 時間込み)。
warm は単一プロセスで `--repeat` 回反復して定常状態を測定。

実運用で言語を指定している場合は、同じ値を `--language ja-JP` などで渡す。
省略時はbackendの言語自動判定を使う。指定値はcold/warm両方の要求と結果の
`config.language`に保持される。既存の`--whisper-profile standard|fast|greedy`比較でも
言語・音声・辞書・量子化をそろえること。`fast`はbeam=1とVADを同時に変更する
実験用profileであり、アプリの既定設定は変わらない。

```powershell
uv run scripts/bench_asr.py --backend faster-whisper --language ja-JP --quantization bf16 --whisper-profile standard --record-device --audio bench_audio/quiet/ja_short_01.wav --repeat 10 --cold-runs 3 --json --output results/ja_standard_new.json
```

出力先の親ディレクトリは先に用意し、比較ごとに新しい出力名を使う。既存結果は
上書きされない。停止後の補正・挿入は再現しないため、ASR単体の結果だけでアプリ全体の
高速化を判断しない。

#### 3.2.1 アプリと同じ要求条件(M2)

既定(`--request-shape production`)では、各`transcribe`要求をアプリの最終認識と同じ形で
送る(`src-tauri/src/asr.rs`の`transcribe_request`)。

- `prompt`と`language`は常に含め、未指定なら`null`とする。`decoding`は送らない。
- キーはserde_jsonと同じ辞書順で直列化し、各行をLFで終える。同じ音声パス・辞書・
  localeなら、アプリが送る行とバイト単位で一致する。
- `prompt`はアプリの`Storage::dictionary_asr_prompt_for`(`storage.rs`)と同じ規則で
  組み立てる。`priority`降順・`surface`昇順に並べ、scopeで絞り込む。faster-whisperは
  surface→alias→手動登録のreadingの順に重複を除き、`, `区切り・200文字以内のhotwordsに
  する。その他のbackendは`reading => surface (aliases: …)`を1行ずつ、1200文字以内で連結する
  (`auto`の項目はsurfaceのみ)。整形には`--backend`の値を使う。
- 辞書は`--dictionary-csv`で渡す。形式はアプリの辞書CSV取込と同じ
  `reading,surface,category,aliases,priority,app_scope`(aliasは`|`区切り、priorityの空欄は0)。
  確定済み候補(`auto`)を再現する場合に限り、末尾に`source`列(`manual`/`auto`)を足せる。
  `--prompt TERM`は、readingとsurfaceがどちらもTERMの手動・global項目として同じ規則で
  整形される(以前のように語のリストをそのまま送るのではない)。
- `app:`・`category:` scopeの項目は、`--app-key`・`--app-category`を指定したときだけ対象に
  なる。指定しない場合は、アプリのEdit/Askや対象アプリ不明の状態と同じく、global項目だけを使う。
- 結果JSONの`config.prompt`には、辞書本文ではなく文字数・項目数・SHA-256を記録する。
  `config.audio_format`には各WAVのサンプルレート・チャンネル数・サンプル幅を記録する。
  アプリが認識に渡すWAVは、faster-whisperなら16 kHz、ほかのbackendなら24 kHzのmono PCM16で、
  ノイズ抑制などの前処理も済んでいる。比較には同じ条件で用意した音声を使う。
- load直後にhealthで問い合わせた実device/compute typeを`config.worker_health`に記録する
  (どの計時にも含めない)。`--record-device`では各要求後の値と、観測した組合せの一覧
  (`config.observed_devices`)も記録する。

`--request-shape legacy`は、この変更より前の要求(未指定の`prompt`/`language`を省略し、
`--prompt`の語をリストのまま送る)を再現する。以前の結果と比べるときだけ使う。
legacyでは`--dictionary-csv`・`--app-key`・`--app-category`・`--live-preview`は使えない。

#### 3.2.2 idle後とライブプレビュー競合

`--idle-gap-s N`は、warmの各要求の前にN秒待つ(計時に含めない)。cold計測には適用しない。
しばらく操作しなかった後の認識を測るときに使う。

`--live-preview`は、warmの各最終要求の前に録音1回分のライブプレビュー要求を流す。
`live_dictation.rs`の直列ループと同じく、`--live-interval-ms`(既定150 ms、`LIVE_INTERVAL`)
待つ→`--live-prep-ms`の音声準備→プレビュー要求を1件送り応答を待つ、を繰り返す。
`--live-recording-s`秒(既定は最終音声の長さ)で停止したとみなす。待機中の停止はすぐ反映される。
準備中やプレビュー要求の処理中に停止した場合は、それが終わるまで最終要求を送らない
(`pipeline/stop.rs`はlive taskをjoinしてから最終認識を始める)。ワーカーは`cancel`以外の
要求を受信順に1件ずつ処理するため、同時送信による競合は起きない。競合は「処理中のプレビュー
の後ろに最終要求が並ぶ」待ちとしてモデル化している。プレビュー要求は最終要求と同じ
`prompt`/`language`を持ち、`--live-preview-greedy`(設定「ライブプレビューを軽量化」に相当)を
指定したときだけ`"decoding":"greedy"`を加える。`--live-preview-audio`で別の音声を使える。

各warm sampleには、停止から最終要求を送れるまでの待ち`live_drain_ms`、それに最終要求の
往復を足した`stop_to_final_ms`、プレビュー回数を記録する。`warm`にはこれらとプレビュー往復
(`preview_wall_ms`)の要約が加わる。プレビューの認識結果はsampleに含めないため、
`score_asr.py`の採点対象にならない。`wall_ms`と`duration_ms`は従来どおり最終要求だけを表す。

制約: アプリは区間判定で不要な反復の認識を省き、発話中の伸びていく区間だけを認識する。
このハーネスは毎回の反復で同じ音声全体を認識するため、プレビュー負荷の上限側を測る。
cold計測にはidle間隔もプレビュー負荷も加えない。

```powershell
uv run scripts/bench_asr.py --backend faster-whisper --language ja-JP --dictionary-csv bench_audio/dictionary.csv --idle-gap-s 30 --live-preview --live-preview-greedy --record-device --audio bench_audio/quiet/ja_short_01.wav --repeat 10 --cold-runs 3 --json --output results/ja_production_live.json
```

### 3.3 VRAM の測り方(nvidia-smi)

ハーネスは VRAM を測らない(GPU 非依存・stdlib 限定のため)。実機で別途記録する。

```powershell
# 計測中、1 秒間隔で使用量をログ(別ターミナルで起動)
nvidia-smi --query-gpu=timestamp,memory.used,memory.total,utilization.gpu `
  --format=csv -l 1 > results\vram_<model>_<quant>.csv
```

- `load` 直後の定常使用量と、`transcribe`(特に 60秒発話)中のピークを記録。
- ローカル LLM 同時常駐時の合算 VRAM も計測(計画書 Phase 0 / §17 の 12GB OOM リスク)。

### 3.4 精度の測り方(CER / WER / 固有名詞)

- **CER(日本語中心)**: 正解文と認識文を文字単位で比較し編集距離 / 正解文字数。
  - 前処理: 空白除去、全角/半角・かな種別の正規化(NFKC)、句読点の扱いを明文化して固定。
- **WER(英語中心)**: 単語単位の編集距離 / 正解単語数。小文字化・記号除去ルールを固定。
- **日英混在保持率(§9.3)**: 混在文で言語切替が保持されているか(英単語が誤って
  かな化されていないか等)を別途スコア化。
- **固有名詞正解率**: 台本に埋め込んだ固有名詞リストの認識率。辞書/プロンプト
  **あり/なし**の差分(改善率)を必ず両方計測する(Phase 0 合格条件)。
- **数字・記号正確性(§9.3)**: 日時・金額・記号を含む文を別カテゴリで集計。

> 精度スコアリングは本ハーネスの範囲外(外部依存なし方針のため)。`bench_asr.py` は
> 認識テキストを `samples[].text` として JSON に保持するので、別スクリプト/ノートブックで
> 正解文と突き合わせて算出する。正解文(台本)はバージョン管理する。

### 3.5 評価音声セットの構築方針

回帰テストのため**評価音声セットは固定**する(計画書 §16.4)。

- **自前収録の台本セット(主)**: 発話長 3/5/15/60秒 × 言語 3 種の台本を用意。
  - 固有名詞(製品名・人名・専門用語)、数字/日時/金額、日英混在を意図的に含める。
  - 録音条件: 静音環境を基準。別途、雑音重畳版を用意し「雑音環境での劣化率」(§9.3)を計測。
  - 24kHz モノラル 16bit WAV を標準フォーマットとする(ワーカー入力に合わせる)。
  - 正解文(台本)とメタデータ(言語・発話長・含有固有名詞)を JSON で併置。
- **公開コーパスの利用方針(補助)**: ライセンスを**事前確認**したうえで、日本語(例:
  Common Voice ja 等)・英語の一般文を補助セットとして利用。**コーパスごとの再配布/
  商用利用条件を `要確認` とし、確認できないものはリポジトリに同梱しない**(参照のみ)。
  ReazonSpeech 等コーパス由来制約のあるモデルは、評価利用条件も併せて確認する。

---

## 4. 判定基準とゲート

### 4.1 デフォルト選定ロジック

各プロバイダーについて **3秒/5秒・warm** の結果を主に見る。

1. **日常入力デフォルト候補**: 「ASR 成分(warm duration)p50 < 800ms」かつ
   「stop→insertion 換算でヘッドライン KPI(p50 < 1.5s / p95 < 2.5s)を満たす見込み」。
   複数満たす場合は **日本語 CER / 固有名詞正解率(辞書あり)** が高い方を優先。
2. **長文・高精度モード担当**: 日常入力 KPI は満たさないが、長文系 KPI(10秒 p50 < 3s)と
   精度(CER / 話者 / タイムスタンプ)で優れるもの(VibeVoice-ASR を想定)。
3. **GPU 制約 / フォールバック**: faster-whisper small (CPU int8) を常設フォールバックに。
4. **OOM ガード**: 12GB でデフォルト構成が OOM するプロバイダーは、デフォルトから外すか
   量子化を必須化する。

### 4.2 ゲート記録(Phase 0 判定サマリ — 記入用)

実機計測後にここを埋める。これが Phase 0 の正式判定記録。

| 項目 | 結果(記入) | 判定(PASS/REVIEW/FAIL) |
|---|---|---|
| 日常入力デフォルトに採用したプロバイダー | _______ | _______ |
| 採用根拠(3秒 warm: ASR p50 / 往復 p50) | _______ ms / _______ ms | _______ |
| ヘッドライン換算(stop→insertion 3秒 warm p50/p95) | _______ s / _______ s | _______ |
| 長文・高精度モード担当プロバイダー | _______ | _______ |
| 10秒 warm p50/p95(長文系 KPI) | _______ s / _______ s | _______ |
| GPU 制約フォールバック | _______ | _______ |
| 12GB OOM 検証 | _______ | _______ |
| 固有名詞 辞書あり/なし改善 | _______ % → _______ % | _______ |
| 判定日 / 実機 GPU | _______ / _______ | — |

### 4.3 結果記入テンプレート(プロバイダー × 条件)

各プロバイダー(と量子化)ごとにこの表を 1 つ用意して埋める。
数値は `bench_asr.py` 出力 + nvidia-smi + 精度スクリプトから転記。

**プロバイダー: `____________`  量子化: `______`  GPU: `____________`  日付: `__________`**

| 条件 | 状態 | ASR duration p50 / p95 (ms) | 往復 wall p50 / p95 (ms) | cold load (ms) | VRAM peak (GB) | CER / WER (%) | 固有名詞(辞書なし→あり) |
|---|---|---|---|---|---|---|---|
| 3秒 / 日本語 | warm | / | / | — | | / | → |
| 3秒 / 日本語 | cold | / | / | | | / | → |
| 3秒 / 英語 | warm | / | / | — | | / | → |
| 3秒 / 日英混在 | warm | / | / | — | | / | → |
| 5秒 / 日本語 | warm | / | / | — | | / | → |
| 5秒 / 英語 | warm | / | / | — | | / | → |
| 5秒 / 日英混在 | warm | / | / | — | | / | → |
| 15秒 / 日本語 | warm | / | / | — | | / | → |
| 15秒 / 英語 | warm | / | / | — | | / | → |
| 15秒 / 日英混在 | warm | / | / | — | | / | → |
| 60秒 / 日本語 | warm | / | / | — | | / | → |
| 60秒 / 英語 | warm | / | / | — | | / | → |
| 60秒 / 日英混在 | warm | / | / | — | | / | → |

---

## 5. Windows 実機での実行手順

### 5.1 前提

- Windows 11、NVIDIA GPU 12-16GB、最新ドライバ + CUDA 対応。
- リポジトリを取得済み、`uv`(または対象の Python 環境)が利用可能。
- 評価用 WAV(24kHz モノラル 16bit)を `bench_audio/` 等に配置済み。
  正解文(台本)も併置(精度算出用)。
- 各 ASR バックエンドの依存とモデルは **別途ユーザーが用意**(本リポジトリでは
  ダウンロードしない)。`asr_worker` に対応バックエンドが実装されていることが前提。

### 5.2 ハーネスのスモーク確認(オフライン可)

実機でもまず mock で配線を確認する(モデル不要)。

```powershell
uv run python scripts\bench_asr.py --backend mock --smoke
uv run python scripts\bench_asr.py --backend mock --smoke --json
```

`--smoke` は一時ディレクトリに 3秒のサイン波 WAV(24kHz/mono/16bit)を生成し、
mock バックエンドで `load → transcribe×repeat → shutdown` を通す。

### 5.3 実計測(例: VibeVoice 4bit)

```powershell
# warm 10 回・cold 3 回、3秒の各言語サンプルで計測し JSON 保存
uv run python scripts\bench_asr.py `
  --backend vibevoice --quantization 4bit `
  --audio bench_audio\ja_3s.wav `
  --audio bench_audio\en_3s.wav `
  --audio bench_audio\mix_3s.wav `
  --repeat 10 --cold-runs 3 --json > results\vibevoice_4bit_3s.json

# 量子化を変えて繰り返す
uv run python scripts\bench_asr.py --backend vibevoice --quantization 8bit ... 
uv run python scripts\bench_asr.py --backend vibevoice --quantization bf16 ...
```

発話長ごと(3/5/15/60秒)・言語ごとにファイルを分けると後段の集計が容易。
VRAM は §3.3 の `nvidia-smi -l 1` を**並行起動**して CSV 保存する。

### 5.4 別バックエンド / カスタム起動コマンド

`asr_worker` 以外のワーカーや独自起動を測る場合は `--worker-cmd` で上書きできる
(JSONL プロトコル互換であること)。`--worker-cmd` 以降の全トークンが起動コマンド。

```powershell
uv run python scripts\bench_asr.py --repeat 10 --audio bench_audio\ja_3s.wav `
  --worker-cmd python -m my_whisper_worker --backend faster-whisper
```

### 5.5 主なオプション

| オプション | 既定 | 説明 |
|---|---|---|
| `--backend` | `mock` | ワーカーのバックエンド(`asr_worker --backend` に渡る) |
| `--quantization` | `4bit` | `load` に渡す量子化 |
| `--whisper-profile` | `standard` | faster-whisperのみ。`standard`はライブラリ既定、実験用`fast`はbeam_size=1とVAD、`greedy`はbeam_size=1のみ(VADなし) |
| `--audio WAV` | (なし) | 計測対象 WAV。複数指定可(繰り返し) |
| `--repeat` | `5` | warm の 1 音声あたり反復回数 |
| `--cold-runs` | `1` | プロセス再起動込み cold 計測の回数 |
| `--language` | (なし) | 音声locale(例 `ja-JP`)。アプリの設定と同じ値を渡す |
| `--prompt TERM` | (なし) | 辞書語。readingとsurfaceがTERMの手動項目としてアプリと同じ形式に整形する。複数可(§3.2.1) |
| `--dictionary-csv CSV` | (なし) | アプリの辞書CSV取込形式の辞書。アプリと同じ形式でpromptに整形する(§3.2.1) |
| `--app-key` / `--app-category` | (なし) | `app:`・`category:` scopeの辞書項目を対象にする対象アプリ情報 |
| `--request-shape` | `production` | `production`はアプリの要求形式。`legacy`は以前のベンチの要求形式 |
| `--idle-gap-s` | `0` | warmの各要求の前の待ち秒数(計時外) |
| `--live-preview` | off | warmの各要求の前にライブプレビュー要求を直列に流す(§3.2.2) |
| `--live-preview-greedy` | off | プレビュー要求に`"decoding":"greedy"`を付ける |
| `--live-preview-audio WAV` | 最終音声 | プレビュー要求に使う音声 |
| `--live-recording-s` | 音声長 | 停止までの録音時間とみなす秒数 |
| `--live-interval-ms` | `150` | ライブ反復間の待ち(`LIVE_INTERVAL`) |
| `--live-prep-ms` | `0` | 反復ごとの音声準備時間の模擬値 |
| `--worker-cmd ...` | (なし) | 既定起動コマンドを上書き(以降の全トークン) |
| `--timeout` | `300` | リクエストごとのタイムアウト秒(ハング防止) |
| `--json` | off | markdown 要約の代わりに JSON を出力 |
| `--output PATH` | (なし) | 結果を新しいUTF-8ファイルへ保存。既存ファイルは上書きしない |
| `--record-device` | off | 各認識後のhealthから実device/compute typeを記録。対応ワーカーが必要 |
| `--smoke` | off | サイン波 WAV を生成し mock で自己完結計測 |

### 5.6 出力の読み方とゲートへの転記

- markdown 要約の **「warm duration (ASR pure)」** = ASR 成分(予算 p50 < 800ms)。
- **「warm wall (round trip)」** = ASR 成分 + IPC/直列化。
- **「cold load」** = 初回ロード時間(§9.3)。
- `--live-preview`時の **「warm stop -> final」** = 停止時に処理中だったプレビューの待ち +
  最終要求の往復。アプリの停止→ASR完了に近い値で、「warm wall」とは別に比較する。
- 要約冒頭のdevice・locale・要求形式・prompt指紋・idle/live条件が一致する結果同士だけを比較する。
- 要約末尾に ASR 成分予算(p50 < 800ms)に対する簡易 PASS/REVIEW ヒントを表示する
  (あくまで ASR スライス単体。最終ゲートは §1.2 の全体予算と §4 で判断)。
- `--json` の数値を §4.3 のテンプレート表へ転記し、§4.2 の判定サマリを確定させる。

### 5.7 異常時の挙動

- ワーカーが応答しない場合、`--timeout` 秒でプロセスを kill し、明確なエラーで終了
  (ハングしない)。ワーカーの stderr 末尾もエラーに含めて表示する。
- ワーカーがエラー応答(`ok:false`)を返した場合は、その `code`/`message` を表示して
  終了コード 1 で停止する(例: `gpu_oom`, `model_load_failed`)。

### 5.8 実験用Fast・Greedyプロファイルの比較

既定の`standard`はライブラリのデコード設定を維持する。`fast`は`beam_size=1`と
`vad_filter=True`だけを変更する実験用の選択であり、アプリの既定には採用していない。
`greedy`は`beam_size=1`だけを変更し、VADは無効のまま音声全体を処理する。
temperature fallbackなど、ほかの設定は`standard`と同じライブラリ既定を維持する。
OpenWhisprのwhisper.cpp既定(greedy)との速度比較や低遅延設定の評価に使う。
ただし一致するのはtemperature 0の探索だけである。`best_of`はライブラリ既定の5のままなので、
fallbackのtemperatureでは5候補をサンプリングする(OpenWhisprは2候補)。fallbackに入る条件も
faster-whisperは圧縮率とlog probability、whisper.cppはentropyとlog probabilityで異なる。
fallbackの多い音声では、この差が速度比較に混ざる。
ワーカー単体では`ASR_WHISPER_DECODE_PROFILE=standard|fast|greedy`で指定できる。
ベンチマークの`--whisper-profile`は環境変数より優先され、選択をJSONのconfigに記録する。

同じモデル・デバイス・量子化・辞書・録音を使い、変更する条件をプロファイルだけにする。
以下は既存の評価録音を使う例。録音は別途準備し、参照台本や既存の結果は変更しない。
CPU比較は`ASR_DEVICE=cpu`、GPU比較は`cuda`で固定し、`auto`の切り替えを比較に混ぜない。

```powershell
$taskRunDir = Join-Path .diagnostics ("decode-profile-" + (Get-Date -Format yyyyMMdd-HHmmss-fff))
New-Item -ItemType Directory -Path $taskRunDir -ErrorAction Stop | Out-Null
$taskPreviousDevice = $env:ASR_DEVICE
try {
  $env:ASR_DEVICE = "cpu"
  uv run scripts/bench_asr.py --backend faster-whisper --audio bench_audio/quiet/ja_short_01.wav --repeat 10 --whisper-profile standard --record-device --json --output (Join-Path $taskRunDir standard.json)
  if ($LASTEXITCODE -ne 0) { throw "Standard benchmark failed" }
  uv run scripts/bench_asr.py --backend faster-whisper --audio bench_audio/quiet/ja_short_01.wav --repeat 10 --whisper-profile fast --record-device --json --output (Join-Path $taskRunDir fast.json)
  if ($LASTEXITCODE -ne 0) { throw "Fast benchmark failed" }
  uv run scripts/bench_asr.py --backend faster-whisper --audio bench_audio/quiet/ja_short_01.wav --repeat 10 --whisper-profile greedy --record-device --json --output (Join-Path $taskRunDir greedy.json)
  if ($LASTEXITCODE -ne 0) { throw "Greedy benchmark failed" }
} finally { $env:ASR_DEVICE = $taskPreviousDevice }
uv run scripts/score_asr.py --bench (Join-Path $taskRunDir standard.json) --ref docs/eval_set/references.json --terms docs/eval_set/terms.txt
uv run scripts/score_asr.py --bench (Join-Path $taskRunDir fast.json) --ref docs/eval_set/references.json --terms docs/eval_set/terms.txt
uv run scripts/score_asr.py --bench (Join-Path $taskRunDir greedy.json) --ref docs/eval_set/references.json --terms docs/eval_set/terms.txt
```

実行順を逆にした比較も新しい結果フォルダーへ保存する。日本語・英語・混在、短文・長文と
静音・雑音の条件でCER/WER、固有名詞、warmのp50/p95を照合してから採否を判断する。
JSONの各sampleにある`device`と`compute_type`が両条件で一致していることも確認する。
healthの時間は認識の計測値に含めない。保存先は計測前に排他的に作成し、計測中の書き込みに
失敗した場合は結果をコンソールにも出力する。計測が途中で失敗すると、予約した空ファイルが
残る場合があるため、再実行には新しい保存先を指定する。
mockのsmokeテストは計測経路だけを確認し、速度・精度の採否根拠には使わない。

### 5.9 manifestによる一括計測と精度比較

固定した`asr-bench-manifest/1`を使うと、音声・参照文・言語・出典を揃えて比較できる。
`audio_path`はmanifestのディレクトリ基準で解決する。各項目の言語を認識要求に使い、
`--language`を明示した場合は全項目をその言語で認識する。`--manifest`は通常の音声指定と併用できない。

```powershell
uv run scripts/bench_asr.py --backend mock --manifest bench_data/manifests/public-smoke-v1.json --repeat 1 --cold-runs 1 --json --output bench_data/mock-smoke-v1.json
uv run scripts/score_asr.py --bench bench_data/mock-smoke-v1.json --manifest bench_data/manifests/public-smoke-v1.json --csv bench_data/score-smoke-v1.csv --json --output bench_data/score-smoke-v1.json
uv run scripts/score_asr.py --bench bench_data/mock-smoke-v2.json --manifest bench_data/manifests/public-smoke-v1.json --baseline bench_data/score-smoke-v1.json --output bench_data/comparison-smoke-v2.md
```

最後の行は、同じmanifestで2回目の計測結果を`bench_data/mock-smoke-v2.json`に保存した後に実行する。
保存先は毎回新しい名前にする。両スクリプトの`--output`とscoreの`--csv`は既存ファイルを拒否する。
scoreの既定出力はMarkdown、`--json`はJSONとし、`--output`を省略すると標準出力へ出す。
rawはUnicode NFCと前後の空白除去だけを行い、内部の空白・句読点・大小文字・文字幅を保持する。
normalizedは既存のCER/WER正規化オプションに従う。
項目別、全体、出典と言語の組み合わせ別に、編集数の合計÷参照長の合計（マイクロ平均）を報告する。
繰り返し認識はwarm優先の最初の文を採点し、認識文の不一致、manifest項目の欠落、余剰音声も報告する。
`ja`で始まる言語は固定でCERのみとし、raw・normalizedともWERは`null`、理由は
`ja: no word segmentation; CER is primary`とする。全体WERも日本語を除いて集計する。
`--baseline`は全体・出典と言語別の指標差（今回−前回）と、backend・model・量子化・profile・device・
manifest名などの条件差を表示する。配列は件数と短いハッシュで示す。実行コマンド、音声パス・形式の一覧、
manifestのローカルパスの差は、条件差とは別の参考情報（machine-local）として表示する。
採点件数と欠落件数の差も表示する。元のbench条件はscore JSONの`config`に保存する。
計測時と採点時のmanifest名・件数が異なる場合や項目が欠落した場合は、標準エラーとJSONの`warnings`で通知する。
調整にはdevを使い、holdoutは最終比較に限定する。
