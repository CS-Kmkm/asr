# Computer Useに依存しないWindows評価

[評価スイート](evaluation_suite.md)から、隔離 VM の試験、ヘッドレス試験、ホスト上の CUDA 測定を実行できます。

## Issue #111 の追加確認

専用 VM `ASR-Eval` を起動し、`ASREvalReal` でデスクトップにサインインします。統合版のリリースビルドと `dist/index.html` を用意したら、ホストの PowerShell 7 で実行します。

```powershell
pwsh -NoProfile -File scripts/run_vm_wave_a_checks.ps1
```

- 端末試験は VM の mintty と PuTTY を開き、固定文が受信側に一度だけ届いたことを確認します。PuTTY の接続先はゲスト内のループバック受信機です。`-Check Terminals` で端末だけを再実行できます。
- Clippy は現在のソースを VM の新規ディレクトリに転送し、`cargo clippy --all-targets --locked --offline -- -D warnings` をゲストで実行します。初回は固定依存をゲストに取得します。取得済みなら `-Check Clippy -Offline` で再実行できます。
- `-AppBinary` で別の対象 exe を指定できます。成否、対象 exe の SHA-256、Clippy が使うソースの SHA-256 は `.ai/evaluation/wave-a-checks-*/` に保存します。終了コード `0` は指定した自動試験の合格、`1` は試験失敗、`2` は準備不足です。

### 物理 USB マイク

ホストから USB マイクの**デバイス自体**を VM に共有します。拡張セッションで「ローカル リソース」から対象 USB デバイスを選びます。音声の「このコンピューターから録音」はリモートオーディオ入力を作るため、抜き差し試験に使うにはゲストから元の USB 録音エンドポイントが見える接続が必要です。共有一覧に機器が出ない場合は、機器単位の USB リダイレクトを確認してください。 [Hyper-V 拡張セッション](https://learn.microsoft.com/en-us/windows-server/virtualization/hyper-v/enhanced-session-mode) · [USB デバイスが共有一覧に出ない場合](https://learn.microsoft.com/en-us/troubleshoot/windows-client/remote/usb-devices-unavailable-remotefx-usb-redirection)

Clippy が通ると、ランナーがゲストで実行する `check_vm_microphone_hotplug.ps1 -List` のコマンドを表示します。USB 録音入力が1つ表示されたら `-List` を外して実行します。複数ある場合は表示された ID を `-DeviceInstanceId` に指定します。画面の指示に従って話し、抜いたまま待ち、同じ USB ポートに再接続してもう一度話します。試験は切断前の録音が残り、同じデバイスで録音を再開できたことを確認します。ログはゲストの `%LOCALAPPDATA%\ASR-Evaluation` に保存します。

専用 VM の準備に使う場所は `C:\ASR-Evaluation\tools` です。Git for Windows Portable 2.56.1 を `GitPortable`、PuTTY 0.85 を `PuTTY`、Visual Studio 2022 Build Tools の C++ ワークロードを `VSBuildTools` に置きます。Rust 1.97.1 と Clippy には `Rust\cargo` と `Rust\rustup` を使います。 [Git for Windows リリース](https://github.com/git-for-windows/git/releases/tag/v2.56.1.windows.1) · [PuTTY 配布元](https://www.chiark.greenend.org.uk/~sgtatham/putty/latest.html) · [Windows の Rust セットアップ](https://learn.microsoft.com/en-us/windows/dev-environment/rust/setup)

評価は、ユーザーが対話端末から起動するローカルツールで行います。権限ダイアログの操作は
ユーザーが先に済ませます。以下は実行手順であり、実機での合格記録ではありません。

## #110 の VM 内挿入スモーク

ホストの PowerShell 7 から、`ASR-Eval` に評価対象のビルドを転送して3ケースを実行できます。
初回準備としてゲストの `ASREval` 標準ユーザーをサインインさせ、初回設定画面を完了してください。
ゲストには VB-CABLE と `C:\ASR-Evaluation\runtime` の `uv.exe`、`python\python.exe` が必要です。
ホストでは、PowerShell Direct 用のゲスト管理資格情報を同じ Windows ユーザーで
`Export-Clixml` したファイルを使います。ゲストの画面や入力を試験中に操作する必要はありません。

```powershell
$out = Join-Path $PWD ('.ai/issue-110-insertion-' + [guid]::NewGuid().ToString('N'))
.\scripts\run_vm_insertion_smoke.ps1 `
  -AppBinary src-tauri/target/release/local-voice-input.exe `
  -AppRevision (git rev-parse HEAD) `
  -ManagementCredentialPath .ai/ASR-Eval-local-admin.credential.xml `
  -OutputDirectory $out
```

`-AppRevision` には評価対象の実行ファイルをビルドしたコミットを指定します。ゲストでは
各ケースで、オフラインの専用プロファイルで asr を新しく起動し、実ホットキーと VB-CABLE で録音します。

- **通常挿入**では、mock ASR の固定文が専用 native EDIT の指定位置に一度だけ入り、前後の文が保持されたことを確認します。
- **フォーカス移動**では、録音中に別欄へ移って停止し、どちらの入力欄も変化せず、アプリがクリップボードへのフォールバックを報告したことを確認します。
- **録音キャンセル**では、録音中に Esc を送り、両欄の保持、キャンセル完了、処理状態への遷移とワーカーへの認識要求がともに観測されなかったことを確認します。

`-IncludeNotepad` を指定すると、4ケース目としてゲストのメモ帳を追加します。ランナーが新しい空テキストファイルを開き、そのウィンドウと入力欄を特定してから録音を始めます。メモ帳の `TextPattern` で固定文との完全一致を確認し、試験終了後は今回開いたメモ帳を閉じます。ゲストに既存のメモ帳が開いている場合は、このケースを `BLOCKED` として報告します。

通常挿入、フォーカス移動、メモ帳では、ASR ワーカーへ渡った録音に合成音が含まれることも検証します。録音データは結果に含めません。
フォーカス移動とキャンセルでは、操作が完了してからさらに2秒間、両方の入力欄を監視します。
ケースごとの合否と実行ファイルの SHA-256 は `result.json` に、起動・終了処理を含む試験全体の結果は `host.json` に記録します。
終了コードは `0` が `PASS`、`1` が `FAIL`、`2` が `BLOCKED` です。

### 実モデルでのメモ帳挿入

上のコマンドに `-RealModelNotepad` を付けると、日本語 FLEURS の固定音声1件を VB-CABLE に再生します。実行前に、ゲストの標準ユーザー `ASREvalReal` でサインインし、初回設定を済ませます。このモードには、ゲストの `C:\ASR-Evaluation\runtime\real-deps\site-packages` に faster-whisper の依存ライブラリ、`C:\ASR-Evaluation\models` に固定リビジョンのモデル一式が必要です。ホストには `practical-ja-smoke-v1` のマニフェストと対象 WAV を用意します。ランナーはモデル5ファイルと元音声、正解文の SHA-256 を照合します。

専用プロファイルでオフラインの faster-whisper を CPU（int8）で動かします。録音が固定音声と一致することは音量変化の相関で確認します。正解文に対する CER を認識指標として記録し、挿入はワーカーの出力とメモ帳の全文が一致するかで判定します。停止ショートカット送信開始から、最初に全文一致を観測するまでの時間を記録します。結果の `real_worker_proof` に CER と録音照合、`acceptance.results` に挿入判定と時間が入ります。

`-RealModelNotepad -RealRepeats 6` では、同じアプリと読み込んだモデルで固定音声を6回測定します。各回で新しい空のメモ帳を使います。`result.json` の `cases[0].trials` に各回の結果が入り、`app_spans_ms.record_ready` で録音開始要求から録音準備完了まで、`stop_to_observed_insertion_ms` で停止からメモ帳への挿入確認までを比較できます。

## 1. 用意するもの

- Windows 11、既存のuv環境。外部アプリの自動操作にはPowerShell 7（`pwsh.exe`）も必要です。
- 読み込み済みのローカルASRモデル。モデルの取得・インストールはランナーから行いません。
- 既に利用できる仮想音声デバイス。既定の再生先は`CABLE Input`、アプリの録音元は
  `CABLE Output`です。物理マイクや音響経路の確認は別途行います。
- [固定音声manifest](eval_set/public_datasets.md)。`audio_sha256`を照合し、同じ音声を再生します。
- テスト専用の設定と文書。開発版は通常、配布版とは別の`dev`データを使います。
  配布版と開発版で同じショートカットを同時登録しないようにしてください。

モデル・設定・デバイス・送信先はアプリ画面で確認します。通常の受け入れ試験では
ローカルASR、Dictateのtoggle、AI補正OFF、自動モード判定OFF、Live text insertion OFFを使います。
Smart Dictate/Edit、ライブプレビュー、stateful streamingのON/OFFは別条件として実行できます。
Hold-to-talkを試す場合は、専用アプリのDictateの設定をONにして`--recording-mode hold`を指定します。
ランナーが設定を書き換えることはありません。初回の録音権限を確認した後、一度録音を止めます。
試験では製品本来の挿入・コピー経路を実行するため、成功時もclipboard-only時も
通常設定に従ってクリップボードやHistoryが更新されます。テスト用データで実行してください。
評価ツール自身はクリップボードを読み書きしません。

## 2. アプリ内の評価出力を有効にする

新しいPowerShellで、評価ごとに新しいディレクトリを作ります。既存アプリの状態を確認し、
評価対象のビルドをこの端末から起動してください。既存インスタンスに接続された場合は、
新しい評価ファイルに状態が出ないためランナーは開始できません。

```powershell
$evalRoot = Join-Path (Get-Location) ('.ai/windows-eval-' + [guid]::NewGuid().ToString('N'))
New-Item -ItemType Directory -Path $evalRoot | Out-Null
$env:ASR_EVAL_EVENTS = Join-Path $evalRoot 'events.jsonl'
$evalRoot
pnpm run tauri dev
```

`ASR_EVAL_EVENTS`は評価用JSONLの出力先を明示する環境変数です。親ディレクトリは先に作成し、
ファイル自体は存在しないパスを指定します。既存ファイルへの追記・上書きはしません。
指定しなければ評価出力はありません。評価後はこの端末を閉じるか環境変数を削除します。

出力は、プロセスID、プロセス内の操作ID、固定の録音状態・結果コード、数値の処理時間だけです。
本文、音声、ウィンドウタイトル、ファイルパス、APIキーは含みません。
明示的な評価中は通常のHistory／利用統計の設定と別に出力します。
250ms間隔のheartbeatとイベントを専用スレッドで書き、キューあふれ・書込み失敗・古いheartbeatは
評価不能として扱います。評価ファイルには操作タイミングが残るため、保存先はローカルにしてください。

## 3. 実機受け入れランナー

別の対話PowerShellでリポジトリを開き、先ほど表示した評価ディレクトリを設定します。
`$profileDb`には**評価対象アプリが実際に使っているDB**を指定します。起動時のDB診断で確認できます。
標準的な開発版の例は`$env:APPDATA\org.localvoice.input\dev\local-voice-input.sqlite3`です。
`$manifest`と`$itemId`は、作成済みmanifestとその中の非空の正解文を持つ音声IDに置き換えます。

```powershell
$evalRoot = Read-Host '先ほどの評価ディレクトリの絶対パス'
$profileDb = Read-Host '評価対象のSQLite DBの絶対パス'
$manifest = Read-Host '作成済みmanifestのパス'
$itemId = Read-Host '評価音声ID'
uv run scripts/windows_acceptance.py --manifest $manifest --item-id $itemId --events (Join-Path $evalRoot 'events.jsonl') --profile-db $profileDb --output-dir (Join-Path $evalRoot 'native-01')
```

実行時の案内に従って準備を確認し、最初に開いた評価用ウィンドウをクリックします。
以降はキーやマウスに触れず、試験の終了を待ちます。通常権限で実行し、アプリとの権限差を避けてください。
前面化が拒否された場合も、強制的な入力や権限の引上げには切り替えません。

| シナリオ | 自動操作と合格条件 |
|---|---|
| `dictate` | 実ホットキーで録音し、録音中の状態を確認してから音声を再生。入力先が非空で、アプリが確定挿入を報告し、正規化CERが指定上限以下 |
| `focus-away` | 録音後に別の登録欄へ移動して停止。元の文・移動先が変化せず、アプリがclipboard-onlyを報告 |
| `selection-change` | 録音後にShift+Leftで選択を変更して停止。元の文が維持され、clipboard-onlyになる |
| `minimize` | 元のウィンドウを最小化して別の登録欄で停止。どちらの文も変化せず、clipboard-onlyになる |
| `cancel` | 録音後にEscでキャンセル。元の文が維持され、アプリがキャンセルを報告 |

追加シナリオは明示指定で実行します。

```powershell
# toggle設定の専用アプリで、停止後の処理中キャンセルを確認する
uv run scripts/windows_acceptance.py --manifest $manifest --item-id $itemId --events (Join-Path $evalRoot 'events.jsonl') --profile-db $profileDb --scenario cancel-processing --output-dir (Join-Path $evalRoot 'processing-cancel-01')
# 専用アプリでDictateのHold-to-talkをONにした後、押下保持からキー解放までを確認する
uv run scripts/windows_acceptance.py --manifest $manifest --item-id $itemId --events (Join-Path $evalRoot 'events.jsonl') --profile-db $profileDb --recording-mode hold --scenario dictate --output-dir (Join-Path $evalRoot 'hold-01')
```

`cancel-processing`は、録音停止後に同じ操作のprocessing状態を確認してEscを送ります。
処理中にEscを送り、キャンセル結果と入力欄の保持が確認できた場合だけPASSです。
Escを送る前に完了していた場合は`processing_window_missed`、通常の貼り付けが先に完了した可能性がある場合は
`processing_cancel_commit_race_unconfirmed`のBLOCKEDになります。別の入力欄への書き込みや、
Esc送信後のキャンセル以外の異常な結果はFAILとして残します。
処理中の観測からEsc送信までに完了する競合は残ります。FAILは試験の期待を満たさなかった結果であり、
アプリがキャンセルを受理した後の製品不具合とまでは断定しません。
この試験もAI補正OFFのDictate条件です。補正・翻訳・Edit・Ask中のキャンセル検証は別に残します。

holdモードでは、録音開始から音声再生完了まで実際にショートカットを保持し、キー解放で停止します。
録音開始待ちは最大3秒で、保持中の追加操作を検知すると中断します。
異常終了時も注入したキーの解放を試み、元の例外の種類と解放の成否を記録します。
解放は録音停止として働き、その後の出力やクリップボード書き込みは観測できない場合があります。
中断後はアプリの完了状態と入力先を確認してください。
holdで`--scenario`を省略すると`dictate`だけを実行します。追加の3件は明示指定できます。

| 指定 | 保持中の操作と期待する結果 |
|---|---|
| `--scenario focus-away` | 移動先の評価欄にフォーカスした状態でキーを解放します。両欄が保持され、アプリ結果が`clipboard_only`ならPASSです。 |
| `--scenario selection-change` | 専用のネイティブEDITで評価用の文を全選択し、選択範囲が変わったことを確認してキーを解放します。両欄が保持され、`clipboard_only`ならPASSです。この結果から、どのガードが挿入を止めたかは分かりません。 |
| `--scenario cancel` | 音声再生後の案内に従い、8秒以内に録音オーバーレイのキャンセルをマウスでクリックします。ランナーは同じ操作の完了を確認するまでキーを保持します。録音中にキャンセルされ、両欄が保持された場合にPASSです。案内が出るまではマウス・キーに触れないでください。 |

`--scenario`を繰り返すと順に実行し、最初のFAILまたはBLOCKEDで止まります。

```powershell
uv run scripts/windows_acceptance.py --manifest $manifest --item-id $itemId --events (Join-Path $evalRoot 'events.jsonl') --profile-db $profileDb --recording-mode hold --scenario focus-away --scenario selection-change --scenario cancel --output-dir (Join-Path $evalRoot 'hold-guards-01')
```

holdでの選択変更は`--target native`で実行してください。キャンセルは保持中の修飾キーの影響を避けるため、
オーバーレイで操作します。時間内に完了が確認できない場合はBLOCKEDとなり、キーを解放します。
その解放で録音が通常停止し、出力が発生する場合があります。
3件とも試験完了まで他のウィンドウへ移動しないでください。想定した評価欄からフォーカスが外れた場合は中断します。
評価ログではキャンセルの発生を確認できますが、どの操作がキャンセルを発生させたかは記録されません。
キャンセルボタンを確実に操作できるよう、
案内を表示する端末と評価ウィンドウ、オーバーレイを見える位置に配置してください。
保存設定と`--recording-mode`が一致しなければ入力前に拒否します。ランナーは設定を変更しません。
フラグを省略した場合は従来どおりtoggle方式の基本5シナリオです。

各試行は、録音開始、音声再生、停止またはキャンセル、同じ操作IDの完了までを直列に実行します。
完了後さらに2秒間、遅れた挿入を確認します。この時間より後の不具合を否定するものではありません。
通常入力のCER上限は既定で0.5です。これは配線・動作確認用の緩い上限であり、製品品質の採用基準ではありません。
`--max-cer 0.1`などで厳しくできます。製品のp50/p95は、多数の固定音声と反復条件を揃えたベンチマークで評価します。
仮想音声デバイスの名前が異なる場合は、`--playback-device`に再生デバイス名の一意な接頭辞、
`--microphone-id`にアプリが保存している録音元IDを指定します。設定DBと実際の評価対象を一致させてください。

### メモ帳・ブラウザ・VS Code

`--target external`を付けると、ユーザーが空のテスト欄を2つ登録します。各登録はEnter後の5秒間に
対象欄をクリックする方式です。ブラウザでは付属の
[`windows_acceptance_target.html`](../scripts/windows_acceptance_target.html)をローカルファイルとして開き、
主な入力先／移動先を登録します。専用のラベル、Document祖先と、そのURLが付属ファイルの`file:` URLであることを照合します。
URLを取得できないプロバイダー、アドレスバー、遠隔ページは受け付けません。
VS Codeでは専用の空文書を使い、Monacoの編集欄であることを確認します。端末欄は除外します。
最小化を含む全試験には、別々のトップレベルウィンドウを用意します。
同じウィンドウの2欄によるフォーカス試験だけなら`--scenario focus-away`を指定できます。

```powershell
uv run scripts/windows_acceptance.py --manifest $manifest --item-id $itemId --events (Join-Path $evalRoot 'events.jsonl') --profile-db $profileDb --target external --output-dir (Join-Path $evalRoot 'external-01')
```

対応候補はメモ帳、VS Code、Edge、Chrome、Firefoxです。UI AutomationのTextPattern、
読取り・選択・フォーカスが使えない欄はBLOCKEDになります。VS Codeのアクセシビリティ設定などは
必要に応じてユーザーが設定してください。ランナーは画面全体を検索せず、登録した欄の参照を保持します。
登録したプロセス・ウィンドウ・要素の同一性とフォーカスを確認してから入力し、
準備中の文が想定と違う場合は上書きしません。パスワード欄・端末・権限ダイアログは対象外です。

### 日本語IMEと録音中のキー入力

日本語IMEを用意したWindowsで、ネイティブ評価欄に対して次を実行できます。

```powershell
uv run scripts/windows_acceptance.py --manifest $manifest --item-id $itemId --events (Join-Path $evalRoot 'events.jsonl') --profile-db $profileDb --scenario ime-composition --output-dir (Join-Path $evalRoot 'ime-01')
```

まず録音前にIME操作を試し、変換の生成と取消しができることを確認します。
録音後もIMEを開いて実キー入力から変換中の文字列を作り、IMM APIで未確定変換が存在することを確認します。
その状態で録音を停止し、clipboard-onlyと元の文の保持を確認します。生成できなければBLOCKEDです。
元のIME開閉状態を復元します。ブラウザ・VS CodeなどのTSF実装については、この試験で合格したとは扱いません。
このシナリオは録音中のキー入力とIMEの組み合わせを確認します。キー入力に対する保護も働くため、
PASSだけでIMEガード単独が正しいと判断しません。IME単独の検証は手動の残件として維持します。

### 結果と中断

結果は新規ディレクトリの`acceptance.json`に出力します。本文やスクリーンショットは保存せず、
結果コード、設定のfingerprint、録音モード・要求シナリオ、音声ID／SHA-256、CER、アプリ内の処理時間を保存します。
新しい結果には操作IDと、ガード試験の入力欄保持の真偽も記録します。本文は保存しません。
OSビルド、ランナーのソースrevisionとdirty状態、評価対象exeのSHA-256、
登録したアプリの名前と取得できた版も記録します。exeのハッシュは開発中のVite配信内容までは識別しません。

- `PASS`: そのシナリオの入力先とアプリ内の結果が合格条件を満たした。
- `FAIL`: 試験を実行できたが、条件を満たさなかった。以降のシナリオは実行しない。
- `BLOCKED`: 前面化、UI Automation、入力、IME、設定、状態出力などを確認できなかった。合格・製品不具合とは断定しない。

未実行のシナリオは結果にありません。全体成功の終了コード0は、要求した全シナリオがPASSの場合だけです。
中断時はキー解放を試みますが、録音トグルを推測して再送しません。
アプリの録音状態を確認し、必要ならユーザーが停止してください。録音が開始済みの場合は、通常のアプリ設定に従って
クリップボードやHistoryが更新される場合があります。評価ツール自身はクリップボードを読み書きしません。

## 4. 受動計測の改修

`bench_e2e_passive.py`はユーザーが製品を操作し、専用入力欄で出力を観測する方式です。
グローバルキーボードフック、クリップボードの読取り／書込み／シーケンス監視はありません。
旧形式の結果・履歴は変更しません。新しい結果は`asr-passive/2`で、旧版のコマンド手順とは異なります。

```powershell
uv run scripts/bench_e2e_passive.py --manifest $manifest --events (Join-Path $evalRoot 'events.jsonl') --app-label 'local-voice-input' --output-dir (Join-Path $evalRoot 'passive-01')
```

他製品では`--events`を省略します。録音開始・停止はユーザーが行います。
制御側は`passive-01/command.json`に次のJSONを順に書き込みます。`sequence`は毎回増やしてください。
同じデスクトップでは、補助ツールが指定秒数後にコマンドを送ります。
`arm`後の準備中は端末へ戻れます。`play`を予約したら5秒以内に専用欄へ戻し、
再生中から観測終了まではフォーカスを維持します。

```powershell
$passiveRoot = Join-Path $evalRoot 'passive-01'
uv run scripts/send_evaluation_command.py --output-dir $passiveRoot --sequence 1 --action arm --item-id $itemId --confirm-idle
# 専用欄に戻り、armの後に録音を開始・確認してから、端末でplayを予約する
uv run scripts/send_evaluation_command.py --output-dir $passiveRoot --sequence 2 --action play --confirm-recording
# 5秒以内に専用欄へ戻る。音声終了後に製品を停止し、出力が落ち着くまで待つ
# 自作アプリは完了状態＋入力欄が1秒安定するとreview。他製品は非空の出力が1秒安定するとreview
# review後は端末へ戻り、実際に製品が停止していることを確認してfinishを送れる
uv run scripts/send_evaluation_command.py --output-dir $passiveRoot --sequence 3 --action finish --confirm-idle --delay 0
uv run scripts/send_evaluation_command.py --output-dir $passiveRoot --sequence 4 --action exit --delay 0
```

`status.json`の`review`は受信側の観測終了です。他製品が停止したことの証明ではありません。
他製品で出力がない場合は音声終了から60秒で観測を閉じます。
自作アプリの完了を300秒以内に確認できなければ`desynced`となり、結果の保存・採点には進みません。
`finish`にはユーザーによる停止確認が必要です。
予約送信が拒否された場合は、状態を確認して新しいsequenceを使ってください。

| 順序 | コマンド例 | 操作 |
|---|---|---|
| 1 | `{"sequence":1,"action":"arm","item_id":"音声ID","operator_confirmed_app_idle":true}` | 製品が停止中で、専用欄が前面の状態で準備 |
| 2 | `{"sequence":2,"action":"play","operator_confirmed_app_recording":true}` | ユーザーが録音開始・権限を確認した後に再生 |
| 3 | `{"sequence":3,"action":"finish","operator_confirmed_app_idle":true}` | 再生終了後にユーザーが録音停止。出力と停止を確認してから結果を保存 |
| 4 | `{"sequence":4,"action":"exit"}` | 完了後に終了 |

再生中の再準備、重複再生、状態不明の終了は拒否します。中断後は録音停止を確認し、
`reset_idle`と`operator_confirmed_app_idle:true`を新しいsequenceで送ります。

`playback_end_to_first_change_ms`は**音声の再生終了から入力欄の最初の変化まで**です。
人の停止操作の時間を含み、`stop→paste`の代用にはしません。途中出力・フォーカス逸脱があればこの値はnullです。
自作アプリでは、同じ操作IDの`app_spans_ms`を別に取得できます。
`app_stop_to_confirmed_delivery_ms`は確定挿入と受信を両方確認できた場合だけの、アプリ側の計測値です。
受信側とアプリ側の時計を引き算しません。クリップボード復元の確認は常に未計測です。
受信内容は`review`時点で固定し、`finish`でも再読取りします。その間に変化していれば
`invalid_late_output`としてCERと遅延をnullにし、採点対象から外します。
不正な状態フレーム・欠落・書込み失敗を検出した場合は、その読取りセッションを再利用しません。
観測器とアプリを新しい評価ファイルで起動し直してください。単なる一時的な読取り失敗や古いheartbeatは、
アプリ停止を確認した上で`reset_idle`から復帰できる場合があります。

## 5. 置き換える範囲と残る確認

通常入力・録音中のフォーカス変更・選択変更・最小化・録音キャンセルは、このランナーのシナリオを
従来のComputer Use操作／手動反復から置き換える実装を用意しました。
対象環境で初回の実機実行が通ることを確認してから受け入れ手順として採用してください。
実モデルの認識品質とStreaming比較は
既存のbatch／replayベンチマークを併用します。

外部アプリのIME、端末、Smart選択文編集のLLM結果、補正・翻訳・Edit・Ask中のキャンセル、Holdでの最小化・処理中キャンセル、
クリップボードの複数形式の復元と競合、物理マイク、見た目や権限ダイアログは
[実機手順](windows_verification.md)を維持します。受け入れ全項目が自動化されたとは扱いません。
セキュリティソフトやOSがランナーを拒否した場合も、無効化・ポリシー変更を自動実行しません。
基本Hold入力と補正OFFの処理中キャンセルは追加シナリオの実装・自動テスト後、対象環境で初回の実機実行を確認してください。
