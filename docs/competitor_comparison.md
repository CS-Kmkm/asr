# OpenWhisprとの日本語ASR比較

Issue #81の対象は、同一の日本語音声を使うローカルエンジンの比較である。
OpenWhisprの起動コードとwhisper.cpp 0.0.11のサーバー実装に基づく条件を以下に記す。
製品全体の速度や使いやすさは未測定であり、実測値は下の「結果」に追記する。

## 比較範囲

| 区分 | 対象 | 入力と終点 | 今回の状態 |
|---|---|---|---|
| A: エンジン | 本リポジトリのfaster-whisper / CTranslate2 | 固定WAV → JSONL応答の認識文 | 既存ハーネスで計測 |
| A: エンジン | OpenWhisprのwhisper.cpp 0.0.11 | 同じWAV → HTTP応答の認識文 | 本ハーネスで計測 |
| B: 製品 | 本アプリ / OpenWhispr | 録音停止 → 対象アプリへの貼り付け完了 | 未測定 |
| B: 製品 | Handy / Amical | 同上 | 未測定 |

音声はFLEURS `ja_jp`のみとし、英語セットは含めない。双方で同じmanifest、
16 kHz・mono・PCM16の音声、参照文、言語、GPU、反復回数を使う。
辞書prompt、ライブプレビュー、LLMによる整文は加えない。
manifestの`ja-JP`はwhisper-serverへの要求で`ja`に変換する。

## 比較するarmと解釈

| arm | 実装 / 重み | デコード | 揃う条件・残る差 |
|---|---|---|---|
| ct2-int8 | faster-whisper large-v3-turbo、`4bit` → CUDA `int8_float16` | standard、beam 5 | モデル系列と音声は共通。量子化とランタイムが異なる |
| ct2-f16 | 同モデル、`bf16` → CUDA `float16` | standard、beam 5 | f16相当だが、重み形式・カーネル・探索実装は異なる |
| ow-default | ggml large-v3-turbo、f16 | greedy、best_of=2 | OpenWhisprの通常起動・要求。beam 5とは探索量が異なる |
| ow-beam5 | 同じggml f16 | 要求に`beam_size=5`を追加 | beam幅を揃える追加arm。探索実装全体の同一性は意味しない |

`4bit`と`bf16`は本アプリ側の設定名であり、このバックエンドで実際に使う型は
それぞれ`int8_float16`と`float16`である。`--record-device`で実際のdeviceと
compute typeを記録し、CPUへの切り替わりがないことを確認する。

OpenWhispr側は`entropy_thold=2.8`、`logprob_thold=-1.25`、
`response_format=json`を送る。promptは送らない。beam指定がなければ
greedy / best_of=2、temperature 0と+0.2のfallback、GPU・flash attention有効という
[whisper.cpp 0.0.11のサーバー実装](https://github.com/OpenWhispr/whisper.cpp/blob/0.0.11/examples/server/server.cpp)
の既定値を使う。`beam_size=1`はgreedyを選択するため、追加armの`--beam-size`は2以上を指定する。
dictation VADは既定で無効なのでVADフラグは付けない。
faster-whisperのcompression ratio、log probability、no-speechなどの閾値は別の
処理段階・fallback判定を持つ。同じ名称や近い数値でも同じ意味とは扱わず、
本比較で数値を合わせる変更は行わない。beam5もサーバー側の他の既定値を維持する。

起動条件の根拠はOpenWhispr
[`main@3dabab6`のwhisperServer.js L159–221](https://github.com/OpenWhispr/openwhispr/blob/3dabab6/src/helpers/whisperServer.js#L159-L221)
である。実行ファイルとモデルは起動前に絶対パスへ解決し、ファイルの存在を検証する。
bare nameもカレントディレクトリ基準であり、PATHからは検索しない。

```text
<exe> --model <path> --host 127.0.0.1 --port <8178..8199> [--threads N] --language ja --max-len 4096
```

環境変数は`CUDA_DEVICE_ORDER=PCI_BUS_ID`。
自動設定のNは`floor(0.75 × os.cpu_count())`を4〜12に制限し、N ≤ 4なら`--threads`を省略する。
明示した`--threads N`は4以下もそのまま渡す。結果の`config.threads`には渡した値を記録し、
省略時はサーバー既定値を使うことを表す`null`とする。
テスト用の末尾引数`--server-cmd ...`は実行ファイル部分を置き換え、
上記のアプリ引数はその後ろに付く。WindowsのPID確認を通すには、指定コマンドが
サーバーを直接execし、listener PIDが起動したプロセスのPIDと一致する必要がある。
スタブはvenvのredirectorを挟まずPython本体を起動する。

## 再現手順（Windows / PowerShell）

以下は実測担当者が実行する手順である。テストでは外部ダウンロードや実サーバーを
使わず、一時ディレクトリの合成WAVとloopback上のスタブだけを使う。
CUDA対応GPU・ドライバー、uv環境、利用可能なfaster-whisper large-v3-turboの
ローカルキャッシュを準備する。本ハーネスにはモデル取得機能を持たせない。

### エンジンと重みの準備

取得元と期待SHA256を以下に固定する。エンジンは
[OpenWhispr/whisper.cppの0.0.11リリース](https://github.com/OpenWhispr/whisper.cpp/releases/tag/0.0.11)、
重みはggerganov/whisper.cppの公開ファイルを使う。取得済みなら、そのファイルを検証する。
URLの内容が変わった場合はチェックを失敗させ、既存ファイルを置換しない。

| ファイル | 取得元 | SHA256 |
|---|---|---|
| エンジンzip（CUDA 12 DLL同梱） | [OpenWhispr/whisper.cpp 0.0.11](https://github.com/OpenWhispr/whisper.cpp/releases/download/0.0.11/whisper-server-win32-x64-cuda.zip) | `5d0ce15234aed4e30c075010c13dd23771fb5c714c48047e287306ab340eea9a` |
| ggml f16重み | [ggml-large-v3-turbo.bin](https://huggingface.co/ggerganov/whisper.cpp/resolve/main/ggml-large-v3-turbo.bin) | `1fc70f774d38eb169993ac391eea357ef47c88757ef72ee5943879b7e8e2bc69` |

新規取得の例（同じPowerShellセッションで以降の手順へ進む）:

```powershell
$comparisonRoot = Join-Path bench_data ("comparison-" + (Get-Date -Format yyyyMMdd-HHmmss-fff))
New-Item -ItemType Directory -Path $comparisonRoot -ErrorAction Stop | Out-Null
$engineZip = Join-Path $comparisonRoot whisper-server.zip
$ggmlModel = Join-Path $comparisonRoot ggml-large-v3-turbo.bin
Invoke-WebRequest -Uri 'https://github.com/OpenWhispr/whisper.cpp/releases/download/0.0.11/whisper-server-win32-x64-cuda.zip' -OutFile $engineZip
Invoke-WebRequest -Uri 'https://huggingface.co/ggerganov/whisper.cpp/resolve/main/ggml-large-v3-turbo.bin' -OutFile $ggmlModel
if ((Get-FileHash -Algorithm SHA256 -LiteralPath $engineZip).Hash.ToLowerInvariant() -ne '5d0ce15234aed4e30c075010c13dd23771fb5c714c48047e287306ab340eea9a') { throw 'Engine SHA256 mismatch' }
if ((Get-FileHash -Algorithm SHA256 -LiteralPath $ggmlModel).Hash.ToLowerInvariant() -ne '1fc70f774d38eb169993ac391eea357ef47c88757ef72ee5943879b7e8e2bc69') { throw 'Model SHA256 mismatch' }
Expand-Archive -LiteralPath $engineZip -DestinationPath (Join-Path $comparisonRoot engine) -ErrorAction Stop
$found = @(Get-ChildItem -LiteralPath (Join-Path $comparisonRoot engine) -Recurse -Filter whisper-server-win32-x64-cuda.exe)
if ($found.Count -ne 1) { throw 'Expected exactly one whisper-server executable' }
$serverExe = $found[0].FullName
```

### 日本語manifestの固定

FLEURSはCC-BY-4.0。取得は明示的にライセンスを確認して行い、以後のbuildと計測は
ローカルファイルで実行する。manifestとlockは別のファイルである。

```powershell
uv run scripts/bench_data.py fetch --source fleurs:ja_jp --accept-license
if ($LASTEXITCODE -ne 0) { throw 'Dataset fetch failed' }
$manifestName = "fleurs-ja-dev-" + (Get-Date -Format yyyyMMdd-HHmmss-fff)
uv run scripts/bench_data.py build --source fleurs:ja_jp --profile dev --seed 20261103 --out $manifestName
if ($LASTEXITCODE -ne 0) { throw 'Manifest build failed' }
$manifest = "bench_data/manifests/$manifestName.json"
uv run scripts/bench_data.py lock --manifest $manifest --output (Join-Path $comparisonRoot manifest.lock.json)
if ($LASTEXITCODE -ne 0) { throw 'Manifest lock failed' }
```

既存lockから再構成するときは`build --lock <lock.json> --out <新しい名前>`を使う。
arm間では再buildせず、同じ`$manifest`を渡す。調整にはdevを使い、holdoutは最終比較に
限定する。holdoutでは別名で`--profile holdout`のmanifestを作り、全armを再実行する。

### 両エンジンの計測と採点

faster-whisperのキャッシュが揃っている状態で`HF_HUB_OFFLINE=1`を指定し、
モデルの自動取得を防ぐ。計測時には他のGPU負荷を止める。
ハードウェア、GPUドライバー、OS、Python・faster-whisper・CTranslate2のバージョン、
キャッシュ済みモデルのrevision、電源設定を実測記録へ添える。

```powershell
$comparisonOldDevice = $env:ASR_DEVICE
$comparisonOldOffline = $env:HF_HUB_OFFLINE
$comparisonOldModel = $env:ASR_MODEL_ID
try {
  $env:ASR_DEVICE = 'cuda'
  $env:HF_HUB_OFFLINE = '1'
  $env:ASR_MODEL_ID = 'large-v3-turbo'
  uv run scripts/bench_asr.py --backend faster-whisper --whisper-profile standard --quantization 4bit --manifest $manifest --repeat 5 --cold-runs 1 --record-device --json --output (Join-Path $comparisonRoot ct2-int8.json)
  if ($LASTEXITCODE -ne 0) { throw 'ct2-int8 failed' }
  uv run scripts/bench_asr.py --backend faster-whisper --whisper-profile standard --quantization bf16 --manifest $manifest --repeat 5 --cold-runs 1 --record-device --json --output (Join-Path $comparisonRoot ct2-f16.json)
  if ($LASTEXITCODE -ne 0) { throw 'ct2-f16 failed' }
  uv run scripts/bench_whisper_server.py --server-exe $serverExe --model $ggmlModel --manifest $manifest --repeat 5 --warmup 1 --model-sha256 1fc70f774d38eb169993ac391eea357ef47c88757ef72ee5943879b7e8e2bc69 --output (Join-Path $comparisonRoot ow-default.json)
  if ($LASTEXITCODE -ne 0) { throw 'ow-default failed' }
  uv run scripts/bench_whisper_server.py --server-exe $serverExe --model $ggmlModel --manifest $manifest --repeat 5 --warmup 1 --beam-size 5 --model-sha256 1fc70f774d38eb169993ac391eea357ef47c88757ef72ee5943879b7e8e2bc69 --output (Join-Path $comparisonRoot ow-beam5.json)
  if ($LASTEXITCODE -ne 0) { throw 'ow-beam5 failed' }
} finally {
  $env:ASR_DEVICE = $comparisonOldDevice
  $env:HF_HUB_OFFLINE = $comparisonOldOffline
  $env:ASR_MODEL_ID = $comparisonOldModel
}
foreach ($arm in @('ct2-int8', 'ct2-f16', 'ow-default', 'ow-beam5')) {
  uv run scripts/score_asr.py --bench (Join-Path $comparisonRoot "$arm.json") --manifest $manifest --json --output (Join-Path $comparisonRoot "$arm.score.json")
  if ($LASTEXITCODE -ne 0) { throw "Scoring failed: $arm" }
}
uv run scripts/score_asr.py --bench (Join-Path $comparisonRoot ow-default.json) --manifest $manifest --baseline (Join-Path $comparisonRoot ct2-int8.score.json) --output (Join-Path $comparisonRoot default-vs-int8.md)
if ($LASTEXITCODE -ne 0) { throw 'Default comparison failed' }
uv run scripts/score_asr.py --bench (Join-Path $comparisonRoot ow-beam5.json) --manifest $manifest --baseline (Join-Path $comparisonRoot ct2-f16.score.json) --output (Join-Path $comparisonRoot beam5-vs-f16.md)
if ($LASTEXITCODE -ne 0) { throw 'Beam comparison failed' }
```

`--baseline`は精度と条件の差（今回 − baseline）を示す。速度の差はbench JSONの
warm `wall_ms`のp50/p95を別に比較する。発話長と項目ごとの差も確認し、
異なる長さの音声を混ぜた単一値から短文の性能を断定しない。
CERはraw・normalizedを併記する。集計は編集数合計 / 参照文字数合計であり、
反復認識は最初のwarm文を採点して不一致を報告する。日本語WERは分かち書きがないため
`null`となり、主指標はCERである。

### 時間の境界と制約

whisper-serverの`cold.load_ms`はプロセス生成からloopbackポートが接続可能になるまで。
Windowsでは`netstat -ano -p TCP`のlistener PIDが起動したプロセスと一致することも確認する。
一致しない間は認識を開始せず、タイムアウトには最後の所有者確認結果（PID不一致や
LISTENING行なしなど）を含める。他のサーバーを計測しない。PID確認を利用できない環境では、
この実行専用のサーバーログに対象アドレス・ポートのlistening/ready行があることを必要とする。
バッファリングで行がまだ出力されていなければreadyとは扱わず、タイムアウトまで待つ。
PID確認の所要時間はTCP接続直後に取得したload時間に含めない。
HTTP要求や初回認識の完了を表す値ではない。起動直後に最初の項目を
`--warmup`回（既定1）認識し、その結果は統計・採点に含めない。その後、各項目を
`--repeat`回認識する。coldの認識samplesは空で、coldのwall/duration統計は件数0となる。
bench_asrは別プロセスでcold認識を取り、warm専用プロセスでは同じwarmup除外を
行わない。faster-whisperバックエンドはload内で無音によるwarmupも行うため、
ウォーム状態とcold loadの定義差も結果に明記する。
CPUスレッド数も各ランタイムの既定値を使い、同数には固定していない。

`wall_ms`はHTTP要求を送る直前から応答本文を読み終わるまで。
multipartの構築とWAV読込は含めない。JSONL側の往復には異なるIPCの費用がある。
[0.0.11の`/inference` JSON応答](https://github.com/OpenWhispr/whisper.cpp/blob/0.0.11/examples/server/server.cpp)
は`{"text": ...}`のみで、処理時間を報告しない。そのため`duration_ms`は常にJSONの`NaN`とする。
不明な処理時間を0やwall時間で埋めない。coldのloadとwarmのwallを製品のstop→paste時間へ
読み替えない。`--device-label`と`--quantization`は記録用であり、GPU設定や重みを変更しない。
`--model-sha256`も記録用であり、ファイルの検証は上の`Get-FileHash`で行う。

ready後のログから`found GPU device ... CUDA0`、`CUDA0 total size`、
`using CUDA0 backend`などを読み、実際のbackendを`config.observed_devices`へ記録する。
CPU backendを示す場合は`cpu`、判別できない場合は空配列と警告を記録する。
記録形式は`[{"device": "cuda"または"cpu", "compute_type": "f16"などの量子化ラベル}]`であり、
ログで重みの数値型まで検証するものではない。`--device-label`と異なる場合はstderrと
結果の`warnings`にも残す。`whisper_decode_profile`は本エンジンに該当しないため`null`となる。

## データとログの扱い

音声、参照文、認識文、manifest、結果JSON、採点結果、サーバーログはgit-ignoredの
`bench_data/`配下に保管し、アップロードしない。公開資料には集計値と条件だけを記す。
CLIの`--output`は既存ファイルを拒否するため、再実行は新しいrunディレクトリで行う。
サーバーのstdout/stderrは出力JSONの隣の一意な`*.server.log`に保存される。
出力省略時はJSONがstdout、ログがOSの一時ディレクトリになるため、実測では
必ず`--output bench_data/...`を指定する。失敗・Ctrl+Cでもサーバーを終了し、
この実行が予約した空の結果ファイルだけを取り除く。診断ログは失敗時にも残す。
空ファイルを取り除く際は作成時のファイル識別情報も照合し、差し替わったファイルは残す。
出力先への書き込みに失敗した場合は、計測済みJSONをstdoutにも出して終了コード1とする。
ready後・warmup後・各要求後・結果出力前にサーバーの生存を確認する。

Windowsでは新しいプロセスグループで起動し、終了時はCTRL_BREAKを試してからterminate、
必要ならkillする。stdout/stderrは同じログへ保存するが、強制終了時に未flushのstdoutが
残る可能性はあるため、所有者確認はPIDを優先する。Windows Job Objectの
`KILL_ON_JOB_CLOSE`も可能な場合に設定し、bench自体の強制終了時にサーバーが残るのを防ぐ。
Jobへの割当をOSが拒否した場合は警告を出し、通常の終了処理を続ける。

## 未測定項目

- 製品E2Eのうち、devセット・holdoutでの計測、toggleモード、VS Code・ブラウザ・ターミナル・IME有効時の
  貼り付け（停止→貼り付けはsmoke 30件・専用EDITだけ測定済み）。
- first partial、ライブ認識、VAD、整文、辞書適用を含むUX。
- CPU使用率、RAM、プロセス単位のVRAM、消費電力、CPU専用動作
  （VRAMは下の結果でGPU全体の外部サンプリング値のみ記録）。
- Handy / Amicalの製品・エンジン比較。
- Nemotron / sherpa-onnx。

## B: 製品E2E

`scripts/bench_e2e.py`は、Windows 11上でアプリのグローバルホットキーを操作し、
固定manifestのWAVをVB-CABLEへ等速再生する。録音停止のキー送信から、ハーネス自身が
所有するネイティブの複数行EDITへの最初の変更（`stop_to_first_paste_ms`）と、
最後の変更（`stop_to_settled_ms`）までを測る。最後の変更後に既定で1,500 msの
無変更期間を確認するが、その待機時間はsettledの値には加算しない。
エンジン比較AのJSONL/HTTP往復とは境界が異なり、その数値を製品E2Eの値として扱わない。
first partial、IMEの未確定文字列、入力中のライブプレビューの応答は測定対象外である。

### 準備と条件

1. VB-CABLEを事前にインストールする。インストールで変わったWindowsの既定の再生・
   録音デバイスは元に戻し、アプリのマイクだけを`CABLE Output`に指定する。
   ハーネスの再生先は`CABLE Input`であり、通常のスピーカーへの再生は行わない。
2. 比較するアプリを一つずつ起動し、対象外のアプリのグローバルホットキーを無効にする。
   ローカルASR、日本語、AI整文オフ、hold操作に揃える。辞書・ライブプレビューもオフにする。
   アプリのバージョン、モデル、量子化、GPU、ホットキー、関連設定を各runに記録する。
   モデルや内部処理を揃えられない場合は`--condition`で違いを明示する。
3. 前節で固定した日本語の`$manifest`を両製品で共用する。準備済みの16 kHz mono PCM16 WAVを
   使い、ハーネスはデータセットやモデルを取得しない。初回は`--limit 1`の別runで動作を確認する。
4. アプリとハーネスは通常権限で動かし、実行中は管理者権限のウィンドウを開かない。
   両製品で変更できる場合は、同じWinキーなしのホットキー（以下では`Ctrl+Shift+Space`）に揃える。
   本アプリを`Ctrl+Win`で使う場合は、対応するコマンドの`--hotkey`も`Ctrl+Win`へ変更し、
   Winキーのマスクに関する下記の制約を条件として記録する。

### 実行と採点

新しい結果ディレクトリを作る。バージョンと条件の入力・アプリ設定はカウントダウン前に済ませる。
`$manifest`は前節で作成したローカルmanifestのパスであり、再buildしない。

```powershell
$e2eRoot = Join-Path bench_data ("e2e-" + (Get-Date -Format yyyyMMdd-HHmmss-fff))
New-Item -ItemType Directory -Path $e2eRoot -ErrorAction Stop | Out-Null
if (-not (Test-Path -LiteralPath $manifest -PathType Leaf)) { throw 'Set $manifest to the frozen Japanese manifest' }
$localVersion = Read-Host 'Local Voice Input version'
$localModel = Read-Host 'Local Voice Input model and quantization'
uv run scripts/bench_e2e.py --manifest $manifest --app-label "local-voice-input-$localVersion" --hotkey 'Ctrl+Shift+Space' --mode hold --playback-device 'CABLE Input' --pre-roll-ms 600 --tail-ms 300 --settle-ms 1500 --timeout-s 30 --warmup 1 --gap-s 2 --condition 'asr=local' --condition 'language=ja' --condition 'cleanup=off' --condition 'live-preview=off' --condition "model=$localModel" --output (Join-Path $e2eRoot local.json)
if ($LASTEXITCODE -ne 0) { throw 'Local product E2E failed' }
```

本アプリのホットキーを無効にしてから、OpenWhisprを同条件に設定して実行する。

```powershell
$owVersion = Read-Host 'OpenWhispr version'
$owModel = Read-Host 'OpenWhispr model and quantization'
uv run scripts/bench_e2e.py --manifest $manifest --app-label "openwhispr-$owVersion" --hotkey 'Ctrl+Shift+Space' --mode hold --playback-device 'CABLE Input' --pre-roll-ms 600 --tail-ms 300 --settle-ms 1500 --timeout-s 30 --warmup 1 --gap-s 2 --condition 'asr=local' --condition 'language=ja' --condition 'cleanup=off' --condition 'live-preview=off' --condition "model=$owModel" --output (Join-Path $e2eRoot openwhispr.json)
if ($LASTEXITCODE -ne 0) { throw 'OpenWhispr product E2E failed' }
foreach ($arm in @('local', 'openwhispr')) {
  uv run scripts/score_asr.py --bench (Join-Path $e2eRoot "$arm.json") --manifest $manifest --json --output (Join-Path $e2eRoot "$arm.score.json")
  if ($LASTEXITCODE -ne 0) { throw "Product E2E scoring failed: $arm" }
}
```

warmupは最初の音声で1回行い、集計・採点から除外する。その後はmanifestの各項目を1回ずつ測る。
toggle専用の製品ではアプリの設定と`--mode toggle`を一致させ、holdと異なる条件として記録する。
holdの停止時刻はキー解放のSendInput呼び出し直前、toggleは2回目の押下呼び出し直前である。
tapの押下と解放の間には40 ms置くが、toggleの停止時刻は押下のままである。
ホットキーにはCtrl・Alt・Winのいずれかが必要で、修飾キーのみならWinを必須とする。
Winキーの解放前、および修飾キーのみの組合せのAlt解放前には、未割当キー`0xE8`を送り、
スタートメニューやAltメニューの表示を抑制する。Winマスクの有無は`config.win_mask_used`に残る。
このマスク入力をユーザー操作とみなす製品では、入力が静まるのを待つガードが延長される可能性がある。
`Ctrl+Win`で測ったrunにはこの制約があり、同じWinなしの組合せによる比較と区別する。

### 安全と結果の読み方

開始前に3秒のカウントダウンと「do not touch keyboard/mouse」を表示する。
実行中はPCを操作せず、管理者権限のウィンドウや通知による割り込みも避ける。
試行前にGetAsyncKeyStateで修飾キーが押されていないことを確認し、押されていれば送信せず中断する。
GetLastInputInfoによる1秒以上の入力アイドルを待ち、10秒以内に満たせなければ中断する。
キー送信前には自身のウィンドウのフォーカスを確認するが、確認とOSの入力配送は不可分ではない。

hold中のフォーカス逸脱では、キー解放の前に最大500 msだけ再フォーカスを試す。
戻せなかった場合もキーは解放するが、ラン全体を中断する。**この解放自体が録音停止であり、
製品がその時点でフォーカスされている別のウィンドウへ貼り付ける可能性がある。**
戻せた試行や停止後の貼り付け時の逸脱は`focus_lost`とexe名だけを記録する。
`focus_lost`が3試行連続した場合はwarmup中でも中断する。
toggleは開始tapから停止tapの完了までに失敗すると、キーを解放してラン全体を中断する。
toggleではキー解放だけで録音が止まるとは限らないため、中断後は対象製品の録音状態を確認する。

各キー解放後にGetAsyncKeyStateで注入キー（マスクを含む）が上がったことを確認し、
残っていればキーアップを1回再送する。それでも下がっていれば明示的なエラーで中断する。
Ctrl+Cではfinallyで解放し、コンソールのCLOSE・LOGOFF・SHUTDOWN通知にもキー解放ハンドラーを登録する。
計測ウィンドウが閉じられたりスレッドが終了したりした場合は中断し、スレッド間メッセージは
SendMessageTimeoutWで待機を制限する。

クリップボードには試行ごとに一意のsentinelを置き、貼り付け後に同じ内容へ戻ったかを
`clipboard_restored`として記録する。sentinel未設定や停止後の変更なしの場合はnullとする。
読み取り失敗は`clipboard_error`に残し、試行のstatusは変えない。falseだけでも試行を失敗にしない。
復元率の分母は`change_events > 0`の全試行で、一致を確認できた試行の割合である。
sentinelには`ExcludeClipboardContentFromMonitorProcessing`形式を付けて履歴への取り込みを抑制する。
終了時は`gap_s`だけ待ってから元のテキストを復元する。
元が画像などの非テキストの場合は復元できず、stderrへ通知して終了時にsentinelを消去する。
音声の複製、スクリーンショット、キー操作ログは保存しない。認識文と結果JSONはローカルに保管する。
`--output`は既存ファイルを拒否する。結果の書き込みに失敗した場合はJSONをstdoutにも出す。

`e2e.counts`は`ok`・`empty`・`timeout`・`focus_lost`・`error`の件数であり、
成功率の分母はwarmupを除く全試行である。期限内に変更がなければtimeout、空文字へ変わって安定すればempty、
変更が続いて安定確認できなければerrorとする。p50/p95は非空で安定したok試行だけを使う。
`wall_ms`はok時の最初の変更までの値、非ok時と不明な`duration_ms`はNaNである。
CERには失敗試行の空文字や途中までの文も含まれるので、成功率とともに読む。
停止直前のEDIT文字列を取得し、最終文字列の接頭辞と一致すれば、その後ろの挿入分だけを`text`とする。
接頭辞が一致しなければ全文を残す。停止前の変更や接頭辞の不一致は`pre_stop_changes`で示す。
EN_CHANGEの時刻と文字列は同じロックで取得し、安定待ちの現在時刻も同じロックで読む。
通知が出ないWM_SETTEXTやUIA ValuePatternによる挿入には、10 ms間隔のWM_GETTEXTLENGTHと文字列の
補助ポーリングを使う。`detected_by`は最初の変更の検出方法（`en_change`または`poll`、変更なしならnull）で、
pollの値にはポーリング間隔による検出遅延が含まれる。
`audio_duration_ms`はWAVの長さ、`play_start_lag_ms`は再生要求からWAV読込・waveOut準備を経て
バッファを送るまでの時間、`playback_ms`は送信からバッファ完了通知までの時間を表す。
Windows build、再生デバイス名、manifest条件、各待機時間もconfigに残る。
Win32 UIテストは`ASR_E2E_WIN32_TESTS=1`を明示した場合だけ動く。通常のテストでは実キー送信・
音声再生・フォーカスを持つウィンドウの作成を行わない。

### 2026-11-03 初回計測（製品E2E B、日本語のみ）

環境はエンジン比較Aと同じ（RTX 3060 12 GB、Windows 11 10.0.26300）。音声は`ja-smoke-v1`
（FLEURS `ja_jp` test、30件、7.9–18.5秒）。各製品をholdモードで1件warmup後に30件。
1回に起動する製品は1つだけで、VB-CABLE経由で同じWAVを実時間再生した。
この計測は本節の安全修正（中断条件、停止後テキストの切り出しなど）の前の版で行った。
停止・最初の貼り付け・安定の時刻定義は修正前後で同じで、各試行の前にEDITを空にしているため
`text`の内容も変わらない。修正後の版でも実機2試行で動作を確認した。

| 製品・バージョン / 設定 | 成功率 | first paste p50 / p95 (ms) | clipboard復元率 | raw / normalized CER | 末尾の幻覚文 |
|---|---:|---:|---:|---:|---:|
| OpenWhispr 1.11.0 / Whisper Turbo ggml f16、CUDA、日本語、AI補正オフ、Ctrl+Win hold | 30/30 | 685 / 785 | 30/30 | 10.04% / 7.32% | 0 |
| 本アプリ main@2ecf602 / faster-whisper turbo `bf16`(float16)、standard、補正オフ、増分最終認識オン（既定）、Ctrl+Shift+Space hold | 29/30 | 547 / 763 | 29/29 | 19.71% / 15.65% | 9 |
| 本アプリ 同上、増分最終認識オフ | 29/30 | 591 / 770 | 29/29 | 14.07% / 8.85% | 0 |

本アプリの音声処理は既定値（ノイズ抑制medium、ゲイン100%、自動ゲインオン）にした。
OpenWhisprは音声処理なし（getUserMediaのecho cancel / noise suppression / AGCはオフ）で、
「マイクを準備しておく」が60秒だった。OpenWhisprはWin chordなので0xE8マスクを使った。

読み方:

- 3条件とも成功した29件で比べると、normalized CERは OpenWhispr 7.39%、本アプリ増分オン 13.34%、
  増分オフ 6.36%。本アプリの停止→貼り付けは29件中、増分オンで25件、増分オフで24件がOpenWhisprより速い。
- **増分最終認識オンでは29件中9件の末尾に「ご視聴ありがとうございました」「ありがとうございました」
  「それではまた」が付いた**（増分オフとOpenWhisprでは0件）。これがCER悪化の主因で、
  無音区間で区切った末尾の短い区間を単独で認識したときのWhisperの幻覚と考えられる。
- 本アプリの失敗1件は両条件とも同じ項目（`…9478046545`）で、RMS 26・ピーク263と他の項目の中央値
  （RMS 2,703）より約40 dB小さい。本アプリは貼り付けも履歴も残さず、OpenWhisprは認識できた。
- OpenWhisprのCER（7.32%）はエンジン単体（6.32%）より高い。仮想ケーブル経由の録音・
  開始と停止のタイミングによる差を含む。30件と少なく、p95の信頼性は低い。

## 結果

### 2026-11-03 初回計測（エンジン比較A、日本語のみ）

環境: Intel Core i5-13400（16スレッド）/ RAM 32 GB / NVIDIA GeForce RTX 3060 12 GB
（ドライバー 610.88）/ Windows 11 10.0.26300 / Python 3.12.11 / faster-whisper 1.2.1 /
CTranslate2 4.8.0 / CT2重み `mobiuslabsgmbh/faster-whisper-large-v3-turbo@0a363e9` /
OpenWhispr whisper.cpp 0.0.11 CUDA（threads 12、ログでCUDA0使用を確認）/ AC電源。

音声: FLEURS `ja_jp` test、seed 20261103、`dev`プール。smoke 30件（発話長 7.9–18.5 秒、
中央値 12.6 秒）と dev 100件（7.1–24.5 秒、中央値 12.7 秒）。holdoutは未使用。
発話はすべて7秒以上で、日常入力の3秒前後の短文の速度はこの結果から断定しない。

#### dev 100件（各項目 warm 1回）

| arm | raw CER | normalized CER | warm wall p50 / p95 (ms) | GPU全体VRAMピーク (MiB) |
|---|---:|---:|---:|---:|
| ct2-int8（本アプリ既定） | 10.15% | 5.52% | 397 / 510 | 2,593 |
| ct2-f16 | 9.96% | 5.48% | 425 / 536 | 3,584 |
| ow-default（OpenWhispr既定） | 10.40% | 5.76% | 305 / 422 | 3,419 |
| ow-beam5 | 10.19% | 5.55% | 508 / 724 | 3,363 |

#### smoke 30件（各項目 warm 3回）

| arm | raw CER | normalized CER | warm wall p50 / p95 (ms) | cold load (ms) |
|---|---:|---:|---:|---:|
| ct2-int8（本アプリ既定） | 11.84% | 6.32% | 407 / 522 | 7,068 |
| ct2-int8 fast（beam 1 + VAD） | 11.47% | 6.66% | 370 / 429 | — |
| ct2-int8 + ライブプレビュー（greedy） | 11.84% | 6.32% | 426 / 551（stop→final p50 589） | — |
| ct2-f16 | 11.53% | 6.13% | 425 / 524 | — |
| ow-default | 11.97% | 6.32% | 287 / 349 | 1,576 |
| ow-beam5 | 11.35% | 6.26% | 462 / 621 | 1,578 |

#### dev 100件の対応比較

（同一項目の差。CER差は normalized、ブートストラップ2000回の95%区間）

| 比較 | CER差（後者 − 前者） | 項目別の勝ち / 負け | 前者が速い項目 |
|---|---:|---:|---:|
| ct2-int8 vs ow-default | +0.25 pt [−0.15, +0.70] | 11 / 10 | 0 / 100 |
| ct2-f16 vs ow-beam5 | +0.08 pt [−0.16, +0.35] | 9 / 8 | 87 / 100 |
| ct2-int8 vs ow-beam5 | +0.04 pt [−0.25, +0.34] | 9 / 7 | 99 / 100 |

読み方:

- 精度はどの組でも95%区間が0をまたぎ、この件数では差を主張できない。
- 既定設定どうしでは、OpenWhisprのgreedy（best_of=2）が本アプリのbeam 5より全100項目で速い
  （p50で約90 ms）。beam幅を5に揃えると本アプリのCTranslate2側が速い。
  速度差の主因はエンジン性能ではなく既定のデコード設定と考えられる。
  本アプリ側のbeam 1・VADなしのarmはまだ無く、同条件のgreedy比較は未測定。
- cold loadの定義は異なる（本アプリ: Pythonワーカー起動・モデル読込・無音warmup、
  OpenWhispr: プロセス起動からポート接続可能まで）。常駐後の体感には影響しない。
- VRAMは`nvidia-smi`を250 ms間隔でサンプリングしたGPU全体の使用量で、
  計測前のアイドル値は約1,156 MiB（デスクトップ等）を含む。プロセス単位の値ではない。
- `wall_ms`の境界は本アプリがJSONL往復、OpenWhisprがHTTP往復で、IPC費用が異なる。
