# ASR 評価セット — docs/eval_set/

## 1. このセットの目的と原則

本セットは ASR モデルの回帰テスト基準となる固定台本・正解データである。

**台本固定の原則**: 一度固定したファイルは変更禁止。変更すると過去計測との数値比較が不能になる。
変更が避けられない場合は、旧バージョンを別名で保存し（例: `references_v1.json`）、
計測結果も新旧を区別して管理すること。

## 2. ファイル一覧

| ファイル | 内容 |
|---|---|
| `scripts_ja.md` | 日本語読み上げ台本（短文×5、中文×3、長文×1） |
| `scripts_en.md` | 英語読み上げ台本（短文×5、中文×3、長文×1） |
| `scripts_mixed.md` | 日英混在読み上げ台本（短文×5、中文×3、長文×1） |
| `references.json` | ファイル名→正解文マッピング（`score_asr.py --ref` 用） |
| `terms.txt` | 固有名詞・技術用語リスト（`score_asr.py --terms` 用） |
| `README.md` | 本ファイル |

## 3. ファイル命名規則

録音 WAV ファイルは以下の規則で命名すること。

```
<lang>_<category>_<seq>.wav
```

| 部分 | 値 | 説明 |
|---|---|---|
| `lang` | `ja` / `en` / `mix` | 言語種別 |
| `category` | `short` / `mid` / `long` | 発話長カテゴリ |
| `seq` | `01` `02` … | 通し番号（ゼロ埋め2桁） |

例: `ja_short_01.wav`、`en_mid_03.wav`、`mix_long_01.wav`

`references.json` のキーと **完全一致** させること（`bench_asr.py` のパス出力は basename で照合される）。

## 4. 録音手順

### 4.1 推奨フォーマット

| 項目 | 設定 |
|---|---|
| サンプリングレート | **24kHz**（`asr_worker` の入力フォーマットに合わせる） |
| チャンネル | **モノラル** |
| ビット深度 | **16bit** |
| ファイル形式 | **WAV** |

変換コマンド例（`ffmpeg` 使用）:

```powershell
ffmpeg -i input.wav -ar 24000 -ac 1 -sample_fmt s16 ja_short_01.wav
```

### 4.2 録音条件

**2条件** で録音すること。条件はファイル名に反映しないが、結果ディレクトリで区別する。

| 条件 | 説明 | 格納先の例 |
|---|---|---|
| 静音環境 | バックグラウンドノイズが低い状態（基準条件） | `bench_audio/quiet/` |
| 雑音環境 | 室内BGM・環境音がある状態（劣化率計測用） | `bench_audio/noisy/` |

### 4.3 読み上げのポイント

- 台本に記載の **想定秒数** を参考に、自然な速度で読み上げること（急ぎすぎない）。
- 各台本を **3回以上** 録音し、最も自然なテイクを採用すること。
- `scripts_mixed.md` の日英混在台本では、日本語部分と英語部分を **それぞれの言語で発音すること**（混在保持率の評価対象のため）。

### 4.4 コード・パス・技術用語の読み方

正解文（`references.json`）にそのまま記載された表記が読み上げ表記である。

主な規約:

| 表記 | 読み方 |
|---|---|
| `VibeVoice` | 英語で「VibeVoice」と発音 |
| `faster-whisper` | 英語で「faster whisper」と発音 |
| `asr_worker` | 英語で「a s r worker」と発音 |
| `bench_audio` | 英語で「bench audio」と発音 |
| `pyproject` | 英語で「py project」と発音 |
| `nvidia-smi` | 英語で「nvidia s m i」と発音 |
| `CTranslate2` | 英語で「c translate two」と発音 |
| `WebSocket` | 英語で「web socket」と発音 |
| `NFKC` | 英語で「n f k c」とアルファベット読み |
| `CUDA` | 英語で「cuda」と発音 |
| `kotoba-whisper` | 英語で「kotoba whisper」と発音 |
| `ReazonSpeech` | 英語で「reazon speech」と発音 |
| `十二ギガバイト` | 日本語で「じゅうにぎがばいと」 |
| `ゼロポイントファイブ秒` | 日本語で「ぜろぽいんとふぁいぶびょう」 |

## 5. 計測ワークフロー

### 5.1 bench_asr.py → score_asr.py の連携

```powershell
# Step 1: ベンチマーク計測（遅延 + 認識テキストの取得）
uv run python scripts\bench_asr.py `
  --backend vibevoice --quantization 4bit `
  --audio bench_audio\quiet\ja_short_01.wav `
  --audio bench_audio\quiet\en_short_01.wav `
  --audio bench_audio\quiet\mix_short_01.wav `
  --repeat 10 --cold-runs 3 --json > results\vibevoice_4bit_short.json

# Step 2: 精度スコアリング（CER / WER / 固有名詞正解率）
uv run python scripts\score_asr.py `
  --bench results\vibevoice_4bit_short.json `
  --ref docs\eval_set\references.json `
  --terms docs\eval_set\terms.txt
```

### 5.2 複数ファイルのバッチ計測例

```powershell
# 全評価ファイル（静音環境）を一括計測
$audio_files = Get-ChildItem bench_audio\quiet\*.wav | ForEach-Object { "--audio $($_.FullName)" }
uv run python scripts\bench_asr.py --backend faster_whisper --repeat 5 `
  @audio_files --json > results\fw_all.json

uv run python scripts\score_asr.py `
  --bench results\fw_all.json `
  --ref docs\eval_set\references.json `
  --terms docs\eval_set\terms.txt
```

### 5.3 辞書あり/なし比較（固有名詞正解率の改善確認）

```powershell
# 辞書なし
uv run python scripts\bench_asr.py --backend vibevoice --repeat 5 `
  --audio bench_audio\quiet\ja_short_05.wav --json > results\no_dict.json

# 辞書あり（--prompt でホットワードを渡す）
uv run python scripts\bench_asr.py --backend vibevoice --repeat 5 `
  --audio bench_audio\quiet\ja_short_05.wav `
  --prompt VibeVoice --prompt faster-whisper --json > results\with_dict.json

# 両方をスコアリング
uv run python scripts\score_asr.py --bench results\no_dict.json `
  --ref docs\eval_set\references.json --terms docs\eval_set\terms.txt
uv run python scripts\score_asr.py --bench results\with_dict.json `
  --ref docs\eval_set\references.json --terms docs\eval_set\terms.txt
```

## 6. 発話長カテゴリと計測マトリクスの対応

`docs/benchmarks.md` §3.1 の計測マトリクスとの対応:

| 台本カテゴリ | 想定発話長 | benchmarks.md の区分 |
|---|---|---|
| `short` | 約3秒 | 3秒発話（ヘッドライン KPI の主対象） |
| `mid` | 約15秒 | 15秒発話 |
| `long` | 約60秒 | 60秒発話（長文 KPI の検証対象） |

> 注: benchmarks.md には「5秒」区分もある。5秒発話は本セットの台本には独立項目として存在しないが、
> `short`（3秒）の台本を少しゆっくり読むか、別途収録することで補完できる。

## 7. terms.txt と references.json の整合性

`terms.txt` に記載した全用語は `references.json` のいずれかの正解文に出現する設計になっている。
整合性は以下のコマンドで機械的に確認すること:

```bash
python - <<'EOF'
import json, pathlib, unicodedata

refs = json.loads(pathlib.Path("docs/eval_set/references.json").read_text(encoding="utf-8"))
all_refs = " ".join(refs.values())

terms_raw = pathlib.Path("docs/eval_set/terms.txt").read_text(encoding="utf-8").splitlines()
terms = [t.strip() for t in terms_raw if t.strip() and not t.strip().startswith("#")]

missing = [t for t in terms if t not in all_refs]
if missing:
    print("FAIL: 以下の用語が references.json に未出現:")
    for t in missing:
        print(f"  - {t}")
else:
    print(f"OK: 全{len(terms)}用語が references.json に出現")
EOF
```
