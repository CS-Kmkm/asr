# 公開データセットによるバッチ評価

この手順は、FLEURS の日本語・英語と LibriSpeech の英語を使って、同じ発話集合で ASR を比較するためのものです。
取得は明示的な `fetch` 実行時だけ行います。`build`、`lock`、計測、採点は取得済みデータを使います。

## ライセンスと出典

| データセット | 対象 | 出典・著者 | ライセンス |
|---|---|---|---|
| FLEURS | `ja_jp` / `en_us` の `test` | [Google / FLEURS](https://huggingface.co/datasets/google/fleurs)、Alexis Conneau ほか | [CC BY 4.0](https://creativecommons.org/licenses/by/4.0/) |
| LibriSpeech | `test-clean`、必要時に `test-other` | [OpenSLR resource 12](https://www.openslr.org/12)、Vassil Panayotov、Guoguo Chen、Daniel Povey、Sanjeev Khudanpur | [CC BY 4.0](https://creativecommons.org/licenses/by/4.0/) |

FLEURS のリビジョンは `70bb2e84b976b7e960aa89f1c648e09c59f894dd` に固定しています。
LibriSpeech は OpenSLR の配布アーカイブを使い、取得時の SHA256 を記録します。
再利用時は上記の出典とライセンスを表示してください。公開元の権利表記も確認してください。
このツールは、選択した音声をモノラル・16 kHz・16 bit PCM WAV に変換します。この変換を変更内容として明示してください。

Common Voice と ReazonSpeech の自動取得は、この段階では実装していません。
配布経路、利用条件、必要な同意・帰属表記の確認が未完了のためです。
利用条件を確認せずに他のデータセットの取得処理へ流用しないでください。

## fetch → build → bench → score

リポジトリのルートから PowerShell で実行します。`uv` 環境に含まれる PyAV (`av`) を使い、依存関係の追加は不要です。
まず公開元とライセンスを確認し、同意する場合のみ取得します。

```powershell
uv run scripts/bench_data.py fetch --source fleurs:ja_jp --source fleurs:en_us --source librispeech:test-clean --accept-license
```

各ファイルについて、ライセンス、URL、概算サイズを先に表示します。
HTTP 応答にサイズがある場合は、本文を取得する前にバイト数も表示します。
既定の取得先は、このリポジトリの `bench_data/downloads/` です。起動時のカレントディレクトリには依存しません。
途中ファイルと記録 JSON を一時領域で準備し、転送完了後に続けて公開します。
ファイルごとの隣接 JSON に URL・サイズ・SHA256 を記録します。
既存ファイルは SHA256 を検証して再利用し、上書きしません。破損時は別の `--cache-dir` を指定してください。
中断位置からの再開には対応していません。
アーカイブと隣接 JSON の片方だけが残っている場合は、エラーに表示された両ファイルの正確なパスを確認してください。
表示されたファイルを削除するか、別の `--cache-dir` へ切り替えて取得し直します。

初回の動作確認用に、各ソースから30件を選択します。

```powershell
uv run scripts/bench_data.py build --source fleurs:ja_jp --source fleurs:en_us --source librispeech:test-clean --profile smoke --seed 20261103 --out public-smoke-v1
uv run scripts/bench_asr.py --backend mock --manifest bench_data/manifests/public-smoke-v1.json --json --output bench_data/mock-smoke-v1.json
uv run scripts/score_asr.py --bench bench_data/mock-smoke-v1.json --manifest bench_data/manifests/public-smoke-v1.json
```

`mock` は配線確認用です。モデルの認識品質を測るときは、計測コマンドのバックエンドとモデル条件を指定してください。
採点時に `--json` を追加すると JSON、`--csv bench_data/score-smoke-v1.csv` を追加すると発話単位の CSV も出力できます。
日本語は CER（文字誤り率）が主指標で、単語分割を行わない WER（単語誤り率）は `null` です。
採点結果には raw と normalized の両方の指標が含まれます。

manifest は `bench_data/manifests/<名前>.json`、音声は `bench_data/audio/<名前>/` に保存します。
音声パスは manifest のディレクトリからの相対パスです。WAV 名は `<id>.wav` です。
既存の manifest・音声集合・lock・計測出力を更新する場合は、新しい名前を選んでください。
選択可能な件数が要求数に満たない場合もエラーになります。
FLEURS の空の正解本文は、警告にスキップ件数を表示して除外します。
長さはデコード結果を保存し、TSV の `num_samples / 16000` との差が50 msを超える場合にエラーにします。

build は音声と manifest を一時領域で準備してから公開します。
通常の失敗時は、その実行が公開した音声を片付けます。既存ファイルは削除しません。
プロセスの強制終了や電源断では、一時ディレクトリや公開途中のファイルが残る場合があります。
取得・build のプロセスが終了したことを確認したうえで、
`bench_data/downloads/<source>/<config>/.download-*` と `bench_data/audio/.build-*` の残存物を確認してください。
削除する場合は、その実行の一時ディレクトリだけを対象にします。
音声だけが公開されて manifest がない場合は、既存音声を保持して新しい `--out` 名を使ってください。

## 開発用集合と最終比較用集合

| profile | pool | 既定の件数（各ソース） | 用途 |
|---|---|---|---|
| `smoke` | dev | 30 | 配線・速度の初期確認 |
| `dev` | dev | 100 | 実装・プロンプト・設定の反復調整 |
| `holdout` | holdout | 100 | 設定を固定した後の最終比較 |

`--count-per-source N` で件数を指定できます。既定の seed は `20261103` で、負の値は指定できません。
グループは `sha256("<source>:<group>")` を整数化し、100で割った余りが50未満なら holdout、それ以外なら dev に固定します。
config・locale はハッシュに含めません。FLEURS は多言語で同じ文IDを共有するため、同じ文はすべての言語で同じ pool に入ります。
この割り当ては seed に依存しません。比較の途中で `--holdout-percent`（既定50）を変えないでください。
`smoke` と `dev` は同じ pool を使うため、相互には重複し得ます。

FLEURS は文IDをグループにし、同じ文の録音からID順で先頭の1件だけを残します。
LibriSpeech は話者IDをグループにし、同じ話者が dev と holdout の両方へ入ることを防ぎます。
候補は長さで三分割し、各区間を `sha256("<seed>:<item.id>")` の16進文字列順に並べて交互に選びます。
LibriSpeech では話者ごとにこの処理を行い、話者を巡回して件数を均等にします。
話者の巡回順も `sha256("<seed>:<speaker-id>")` の16進文字列順です。
最初に選ぶ長さの区間を話者ごとにずらし、少数件でも短い発話だけに偏らないようにします。
出力はID順で、Python の random モジュールの実装には依存しません。

holdout の結果を見ながら調整を続けると最終比較として使えなくなります。
反復調整には dev を使い、holdout はモデル・設定・正規化条件を固定した後に使ってください。
両集合の同じ原文や内容が完全に独立していることまで保証する仕組みではありません。

## lock の保存と別PCでの再構築

実データの音声・正解本文・manifest はコミットしません。`bench_data/` 全体は git の除外対象です。
共有するのは、発話ID、ハッシュ、選択条件、出典情報だけを含む lock です。
lock に正解本文や `audio_path` は含めません。

```powershell
uv run scripts/bench_data.py lock --manifest bench_data/manifests/public-smoke-v1.json --output docs/eval_set/public/public-smoke-v1.lock.json
```

`docs/eval_set/public/` はこのコマンドが必要に応じて作成します。
生成した lock の差分に本文・ローカルパス・音声がないことを確認し、通常のレビューを経て lock だけをコミットしてください。
一度比較に使った lock は書き換えず、新しい版のファイル名を作ります。
ツール自身はコミットを行いません。

別PCでは、同じソースをライセンス同意のうえ取得してから再構築します。

```powershell
uv run scripts/bench_data.py fetch --source fleurs:ja_jp --source fleurs:en_us --source librispeech:test-clean --accept-license
uv run scripts/bench_data.py build --lock docs/eval_set/public/public-smoke-v1.lock.json --out public-smoke-v1
```

`--lock` はソースや選択条件の上書きオプションと併用できません。
発話が欠けている場合や、元のアーカイブ内音声の `source_audio_sha256` が違う場合は失敗します。
アーカイブ全体のハッシュは取得物の記録です。lock とローカルの値が異なる場合は標準エラーに通知します。
再梱包によって異なっても、個別音声のハッシュが一致すれば再構築できます。
変換後の `audio_sha256` は PyAV / FFmpeg の版で変わり得るため、元音声のハッシュを再現性の判定に使います。
計測結果には使用した lock、バックエンド、モデル、量子化、デバイス、ソフトウェア版も併記してください。
