# 公開音声による日本語ASR評価

この手順は、FLEURS の日本語（既定）で同じ発話集合を使い ASR を比較するためのものです。本プロジェクトの評価対象は日本語であり、FLEURS 英語と LibriSpeech は必要な場合だけ `--source` で明示指定します（既定では取得・構築しません）。
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

## 固定済みの日本語セット（Issue #86）

人が同じ文章を読み直さず、基本的な日本語認識を反復確認するためのセットです。
厳密なモデル順位や未知話者での性能を保証するものではありません。
FLEURS（説明文の読み上げ）に、感情発話・非言語発声を含む別ドメインの
[JVNV公式配布](https://sites.google.com/site/shinnosuketakamichi/research-topics/jvnv_corpus)を追加しました。
JVNVの著者は Detai Xin、Junfeng Jiang、Shinnosuke Takamichi、Yuki Saito、Akiko Aizawa、Hiroshi Saruwatari、
ライセンスは [CC BY-SA 4.0](https://creativecommons.org/licenses/by-sa/4.0/) です。
[原論文](https://arxiv.org/abs/2310.06072)も参照してください。
公式 `jvnv_ver1.zip`（2023-10-21、約1.78 GB）を使い、4話者・6感情の `regular` セッションを対象にします。
`free` は非言語発声を話者が選ぶため、共有台本を正解として扱わず対象外にしました。
台本CSVの第3列を改変せず参照文として使います。笑いなどの文字表現も正解に含まれるため、
実用上の読みやすさとCERは一致しないことがあります。
音声の変換はFLEURSと同じmono 16 kHz PCM16です。音声・本文・派生音声は再配布しません。

コミット済みlockは `docs/eval_set/public/practical-ja-{smoke,dev,holdout}-v1.lock.json` です。
smokeは各ソース30件（計60）、devとholdoutは各ソース100件（それぞれ計200）です。
JVNVは話者IDでpoolを固定し、各pool内で話者巡回・発話長の三分位を使って選択します。
話者間で共通台本があるため、pool間の文章独立性までは保証しません。
FLEURSは文IDで分離しますが、話者IDが提供されないため話者重複の防止は保証できません。
難易度ラベルもありません。発話長とドメインの偏りを確認するための基本セットとして使います。

```powershell
uv run scripts/bench_data.py fetch --source fleurs:ja_jp --source jvnv:ver1 --accept-license
uv run scripts/bench_data.py build --lock docs/eval_set/public/practical-ja-smoke-v1.lock.json --out practical-ja-smoke-local-v1
uv run scripts/bench_asr.py --backend faster-whisper --manifest bench_data/manifests/practical-ja-smoke-local-v1.json --quantization bf16 --repeat 2 --cold-runs 1 --record-device --model-revision VERIFIED_COMMIT --json --output bench_data/batch-new.json
uv run scripts/score_asr.py --bench bench_data/batch-new.json --manifest bench_data/manifests/practical-ja-smoke-local-v1.json --json --csv bench_data/score-new.csv --output bench_data/score-new.json
uv run scripts/score_asr.py --bench bench_data/batch-new.json --manifest bench_data/manifests/practical-ja-smoke-local-v1.json --output bench_data/score-new.md
```

`VERIFIED_COMMIT`は実際に確認したモデルrevisionに置き換えます。省略時は未検証（`null`）です。
既存モデルは `ASR_MODEL_ID` でローカルsnapshotを指定できます。`ASR_DEVICE=cuda`などの要求と
`--record-device`で記録した実device・compute typeを区別してください。
`runtime`はfaster-whisperで `ctranslate2`、他のラッパーでは `--runtime nemo` / `--runtime speech.cpp`などで明示します。
runtimeラベルは申告値であり、実deviceの代わりではありません。CUDAとVulkanをまとめません。
batch時間の表は `bench_asr.py` のMarkdown出力、採点表は `score_asr.py` で生成できます。
`--baseline`は以前の採点JSONとの差と条件差を表示します。warm反復を増やしても独立な音声件数は増えません。
少数件のp95からモデルの優劣を断定しないでください。補正LLMを通さないRaw ASRが対象です。
補正後を評価する場合は、同じmanifestに対する別結果として採点し、Rawと分けて表示します。
専門語は `--terms`、数値・否定・句読点はraw/normalizedの差と発話別CSVから確認できます。
クラウド費用・VRAM/CPU計測は自動化していません。必要な実測は別途記録します。

## 実時間streaming replay

`bench_asr.py --live-preview` はbatchの反復要求です。streaming engineの評価には
`scripts/bench_streaming.py` を使います。WAVの元サンプル数を時刻として、各PCMチャンクの
末尾まで収録できた時点に送信します。100 ms設定なら最初の送信は100 ms後です。
端数チャンクとpauseを含め全サンプルをそのまま送り、早送り・無音除去はしません。
部分結果の受信は送信・待機と独立したスレッドで時刻を記録します。

```powershell
# 配線確認用。認識品質の評価には使いません。
uv run scripts/bench_streaming.py --manifest bench_data/manifests/practical-ja-smoke-local-v1.json --chunk-ms 100 --output bench_data/stream-mock-new.json --adapter-cmd uv run scripts/bench_streaming.py --mock-adapter
# engine導入後は末尾をそのローカルJSONLアダプタの起動コマンドへ置き換えます。
uv run scripts/score_asr.py --bench bench_data/stream-mock-new.json --manifest bench_data/manifests/practical-ja-smoke-local-v1.json --output bench_data/stream-score-new.md
```

engineのロードは `ready` までに完了させ、replayの計時から除外します。
JSONには発話別に `first_partial_ms`、`first_stable_ms`（初回表示、未発生はnull）、
`flush_to_final_ms`、`replay_rtf`（replay開始→final受信 / 音声長）、各チャンクの
元時刻・送信時刻・送信完了時刻・遅れ、各表示イベントの受信時刻・累積文字列を保存します。
`replay_rtf`は実時間待機を含み、推論だけのRTFではありません。`wall_ms`もreplay全体です。
worker処理時間は不明なので `duration_ms=null` です。`stable_retractions`は確定済みprefixの撤回回数です。
重複・脱落は最終CERおよび表示イベント列で確認します。音響上の単語時刻との対応は推定しません。
送信が詰まっても元時刻は変更しません。`lateness_ms`は予定時刻から送信処理開始までの遅れ、
`write_completed_lateness_ms`は予定時刻からローカルpipe書き込み完了までの遅れです。
後者にはシリアライズ・スレッド起動・pipe書き込み待ちを含みますが、engine側の受領時刻ではありません。
部分／最終結果の受信時間にはOS・パイプ・アダプタ転送の遅延も含みます。
`--timeout`は起動応答、入力pipe停止、flush→final待ちの上限です。
失敗時は最終結果ファイルを作らず、成功したJSONだけを上書きなしで公開します。

アダプタは標準入力／標準出力のUTF-8 JSONLで通信し、診断は標準エラーに出します。
音声と参照文をクラウドへ送らないローカルアダプタを使ってください。参照文は渡しません。
各発話について次の順に処理し、すべての応答に要求と同じ `stream_id` を付けます。

1. `start`: `sample_rate`, `channels=1`, `sample_width=2`, `language` を受信。
   `ready`イベントに `backend,runtime,model,model_revision,quantization,device,compute_type` の文字列を返します。
   未確認の属性は `unknown` と明示してください。前発話の状態をリセットします。
2. `audio`: `offset_frames`, `frames`, `pcm_s16le`（base64、little-endian符号付き16bit）を受信。
   `partial` / `stable`イベントに `text`（その時点の累積全文）を随時返します。
   deltaしか持たないengineはアダプタで累積化します。出力読取りは常時稼働しているため、送信待ち中も表示できます。
3. `flush`: engineを終端まで処理し、`final`イベントに最終累積 `text` を返します。
   エラー時は `event=error,message=...` を返します。終了時はプロセスを停止します。

同一プロセスで複数発話を逐次評価します。`start`ごとのidentityを保存します。
現時点では模擬アダプタで境界・時刻・採点・異常系を検証済みです。
本物のstreaming engineの接続と性能評価は #82 で行います。模擬結果でstreaming精度や速度を主張しません。

## 学習データ重複の扱い

2026-11-09に確認した一次情報に基づくリスク記録です。未記載を「重複なし」とは扱いません。

| モデル／集合 | 分かっていること | 比較での扱い |
|---|---|---|
| [Haas公式モデルカード](https://huggingface.co/third-intelligence/Haas-ja-v1.0) | ReazonSpeech / Common Voice / YODAS を含む学習データを使用。開発元評価でFLEURS/JVNV等を使用 | ReazonSpeech/Common Voiceを主順位から除外。FLEURS/JVNVも独立性の保証ではなく基本動作・ドメイン別結果として扱う |
| [Parakeet Japanese](https://huggingface.co/nvidia/parakeet-tdt_ctc-0.6b-ja) | ReazonSpeech v2.0で学習。公開カードにJSUT/Common Voice/TEDxJPの評価あり | ReazonSpeechは主順位から除外。FLEURS/JVNVへの重複は未確認 |
| [Whisper](https://github.com/openai/whisper/blob/main/model-card.md) / faster-whisper | 大規模Web音声で学習、全学習項目は公開されていない | FLEURS/JVNVへの重複は不明。変換モデルのrevisionも別途記録 |
| 将来のNemotron/Qwen3等 | 今回は学習コーパスを検証していない | engine/model導入時に確認するまで重複不明 |

devで設定を調整し、holdoutは条件を固定した後だけ使います。holdoutを見て調整した場合は
独立な最終評価とは呼びません。FLEURSとJVNVのスコアをドメイン別にも確認し、
英語LibriSpeechを日本語主スコアへ混ぜません。自然会話、日英混在、技術語、小声の網羅は保証しません。
不足分は利用同意を確認したローカル音声と同じmanifestの基本形式で追加でき、Gitにpushしません。

## 実装・検証記録（2026-11-09）

PR #90でFLEURS/LibriSpeechの取得・manifest・lock生成・batch採点・baseline比較まで実装済みでした。
今回、JVNV取得、配布可能な固定lock、参照文ハッシュ検証、streaming replayと採点レポート、
runtime/model revisionの記録、上記の利用条件と限界を追加しました。

ローカルRTX 3060（12 GB）で `practical-ja-smoke-v1` の60音声をfaster-whisperで実認識しました。
モデルは `mobiuslabsgmbh/faster-whisper-large-v3-turbo`、revisionは
`0a363e9161cbc7ed1431c9597a8ceaf0c4f78fcf`、profileはstandard、言語はmanifestのja-JP、
量子化引数はbf16、実際のhealthは `cuda / float16` です。warm各2回、coldプロセス1回です。

| 対象 | 音声件数 | Raw CER | 正規化CER |
|---|---:|---:|---:|
| FLEURS | 30 | 11.53% | 6.13% |
| JVNV regular | 30 | 10.58% | 4.55% |
| 全体（編集数／参照文字数） | 60 | 11.14% | 5.49% |

warm120要求のwall p50は509.12 ms、p95は615.36 ms、worker p50は508.50 msでした。
採点は音声ごとの先頭warm結果を使い、反復結果を独立標本として水増ししていません。
元のJSON・CSV・Markdownはローカルの `bench_data/results/issue-86-practical-ja-{batch,score}-v1.*` に保存しました。
音声・認識文・正解本文はGitに含めません。この単一条件は基盤の疎通確認値でありモデル間比較ではありません。

smoke lockから60件を再構築し、ID・元音声SHA256・変換後SHA256・参照文の一致を確認しました。
dev200件とholdout200件のID重複なし、JVNVの話者重複なしも確認しました。
9.3秒の公開WAV一件でreplay→模擬アダプタ→Markdown採点を結合確認し、
最初のpartialは約100.6 ms、replay RTFは約1.000でした。これは配線確認値であり実ASRのstreaming性能ではありません。

## fetch → build → bench → score

リポジトリのルートから PowerShell で実行します。`uv` 環境に含まれる PyAV (`av`) を使い、依存関係の追加は不要です。
まず公開元とライセンスを確認し、同意する場合のみ取得します。

```powershell
uv run scripts/bench_data.py fetch --source fleurs:ja_jp --accept-license
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
uv run scripts/bench_data.py build --source fleurs:ja_jp --profile smoke --seed 20261103 --out public-smoke-v1
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
lock に正解本文や `audio_path` は含めません。新しいlockには参照文のUTF-8 SHA256も保存し、
再構築時の台本変更を検出します（旧lockは従来どおり参照文ハッシュなしで読めます）。

```powershell
uv run scripts/bench_data.py lock --manifest bench_data/manifests/public-smoke-v1.json --output docs/eval_set/public/public-smoke-v1.lock.json
```

`docs/eval_set/public/` はこのコマンドが必要に応じて作成します。
生成した lock の差分に本文・ローカルパス・音声がないことを確認し、通常のレビューを経て lock だけをコミットしてください。
一度比較に使った lock は書き換えず、新しい版のファイル名を作ります。
ツール自身はコミットを行いません。

別PCでは、同じソースをライセンス同意のうえ取得してから再構築します。

```powershell
uv run scripts/bench_data.py fetch --source fleurs:ja_jp --accept-license
uv run scripts/bench_data.py build --lock docs/eval_set/public/public-smoke-v1.lock.json --out public-smoke-v1
```

`--lock` はソースや選択条件の上書きオプションと併用できません。
発話が欠けている場合や、元のアーカイブ内音声の `source_audio_sha256` が違う場合は失敗します。
アーカイブ全体のハッシュは取得物の記録です。lock とローカルの値が異なる場合は標準エラーに通知します。
再梱包によって異なっても、個別音声のハッシュが一致すれば再構築できます。
変換後の `audio_sha256` は PyAV / FFmpeg の版で変わり得るため、元音声のハッシュを再現性の判定に使います。
計測結果には使用した lock、バックエンド、モデル、量子化、デバイス、ソフトウェア版も併記してください。
