# Windows 実機手動検証ガイド

> **対象者**: テスター / テックリード
> **目的**: WSL2 開発環境では再現できない Windows 固有の動作を実機で手動確認する。
> 本ドキュメントはコードと 1:1 で対応するテストスクリプトであり、
> 実装が存在しない動作は記載しない。

---

## 1. 位置づけ / 前提

### 1.1 このドキュメントが必要な理由

本リポジトリの開発・CI 環境は WSL2 (Linux) である。
以下の機能は Windows API に依存しており、Linux ビルドではコンパイルのみ可能で
実動作の確認ができない。

| 領域 | Windows 固有 API |
|---|---|
| マイクキャプチャ | cpal + WASAPI |
| テキスト挿入 | UI Automation / SendInput / クリップボード Win32 |
| IME ガード | UI Automation `TextEditPattern::GetActiveComposition` |
| セキュア入力検出 | `ES_PASSWORD` スタイル / UIAutomation `IsPassword` / `CredentialUIBroker` 等 |
| UWP アプリの解決 | `ApplicationFrameWindow` の子 `Windows.UI.Core.CoreWindow` の所有プロセス |
| 整合性レベル検査 | `GetTokenInformation(TokenIntegrityLevel)` |
| クリップボード履歴除外 | `ExcludeClipboardContentFromMonitorProcessing` 等登録フォーマット |
| 自動起動 OS 登録 | `tauri-plugin-autolaunch`（`HKCU\...\Run` など） |

### 1.2 前提ハードウェア / OS

| 項目 | 要件 |
|---|---|
| OS | Windows 11（WebView2 標準搭載） |
| GPU | NVIDIA CUDA 対応、VRAM 12 GB 以上 — **VibeVoice バックエンド使用時のみ必須** |
| マイク | システムデフォルト、または任意の録音デバイス |
| ビルドツール | Rust stable (MSVC ターゲット)、Visual Studio 2022 Build Tools、Node.js 20+、Python 3.10–3.13 |

**faster-whisper バックエンドは CPU 動作のため GPU 不要。**
**mock バックエンドはプロトコル確認専用で GPU・モデル不要。**

### 1.3 ビルド / 実行コマンド (README.md より)

```powershell
# 依存インストール
pnpm install
py -3.10 -m venv .venv
.\.venv\Scripts\Activate.ps1
pip install -e .                          # デフォルト (faster-whisper、CPU対応・軽量)
# pip install -e ".[vibevoice]"           # VibeVoice バックエンドを使う場合 (GPU必須)
$env:ASR_PYTHON = "$PWD\.venv\Scripts\python.exe"

# 起動
pnpm run tauri dev

# mock バックエンドでプロトコル確認のみ (モデル不要)
$env:ASR_WORKER_BACKEND = "mock"
$env:ASR_WORKER_MOCK_TEXT = "テスト認識テキスト"
pnpm run tauri dev
```

---

## 2. 共通セットアップ（全シナリオ共通、1 回だけ実施）

以下の手順を順に完了させてから各シナリオへ進む。

| # | 操作 | 期待結果 |
|---|---|---|
| 1 | リポジトリを Windows 実機へ clone し、上記ビルドコマンドを実行する（`pip install -e .` でデフォルトの faster-whisper スタックがインストールされる） | `pnpm run tauri dev` でアプリウィンドウが開く |
| 2 | バックエンドを確認・選択する。デフォルトは **faster-whisper (CPU対応)**。GPU ありで長文/高精度用途なら Settings → ASR backend で **VibeVoice (GPU)** を選ぶ（事前に `pip install -e ".[vibevoice]"` が必要）。GPU なしで検証のみなら **mock (開発用)** を選ぶ | Settings 画面に選択が反映される |
| 3 | ASR モデルを読み込む。faster-whisper の場合: **Load ASR model** ボタンを押す（または初回 transcribe 時に自動ダウンロード）。VibeVoice の場合: Model & GPU ページ → **Load 4-bit model** ボタン | Model ステータスが `ready` になる |
| 4 | Setup ページ → マイクをプルダウンで選択（「System default」でも可） | マイクが設定される |
| 5 | Setup ページ → ホットキーを確認（デフォルト `Ctrl+Shift+Space`）。変更する場合は入力欄を書き換える | ホットキーが再登録される |
| 6 | メモ帳などのテキスト入力欄をフォーカスしてホットキーを押し、短文を発話してもう一度ホットキーを押す | テキストが挿入される、またはクリップボードに残る（mock の場合は `ASR_WORKER_MOCK_TEXT` の内容） |

---

## 3. シナリオ 1: 個人辞書の効果

**対応要件**: R-014（ユーザー辞書）、R-011/R-004（辞書語を ASR プロンプトへ渡す）

### 3.1 仕組みの確認

録音開始時に `storage.dictionary_asr_prompt_for()`（`src-tauri/src/storage.rs`）が、
前景アプリのスコープに一致する辞書項目から ASR ヒントを作り、ASR ワーカーへ
`prompt` パラメータとして送信する。形式はバックエンドで異なる。

- **faster-whisper バックエンド**: ホットワード。全項目の surface（優先度順）→ alias →
  手動項目の reading の順に、大文字小文字を区別せず重複を除き、カンマ区切りで
  最大 200 文字まで入れる。
- **その他のバックエンド（VibeVoice など）**: 1 項目 1 行の `reading => surface`
  （alias があれば `(aliases: ...)` を付ける。自動追加項目は surface のみ）を、
  最大 1200 文字まで改行区切りで入れる。

### 3.2 使用する固有名詞（`docs/eval_set/terms.txt` より 5 語を抜粋）

| 語 | 想定誤認識 |
|---|---|
| VibeVoice | 「バイブボイス」など |
| faster-whisper | 「ファスターウィスパー」など |
| アカリテック株式会社 | 「明かりテック」など |
| 鈴木プロダクトマネージャー | 読み誤り |
| CTranslate2 | 「CTranslate ツー」など |

### 3.3 手順

**ベースライン測定（辞書なし）**

| # | 操作 | 期待結果 | 記録欄 |
|---|---|---|---|
| 1 | Dictionary ページを開き、エントリが 0 件であることを確認する | 「No dictionary entries yet」が表示される | |
| 2 | メモ帳を開いてフォーカスする | — | |
| 3 | `docs/eval_set/scripts_ja.md` の台本 `ja_short_05`（「VibeVoice のバージョンは最新です」）を発話する | テキストが挿入される | |
| 4 | 同様に `ja_mid_02`（faster-whisper、十二ギガバイト等を含む文）を発話する | テキストが挿入される | |
| 5 | 挿入されたテキストをコピーし、固有名詞の認識結果を記録する | 辞書なしの誤認識を確認 | |

**辞書登録**

| # | 操作 | 期待結果 |
|---|---|---|
| 6 | Dictionary ページ → フォームに以下を入力し「Add entry」を押す（5 語分繰り返す）: Reading=読み仮名、Surface=正しい表記、Aliases=省略形など（カンマ区切り）、Priority=10 | エントリ一覧に追加される |

入力例:

| Reading | Surface | Category | Aliases | Priority |
|---|---|---|---|---|
| ばいぶぼいす | VibeVoice | 製品名 | VoiceASR | 10 |
| ふぁすたーうぃすぱー | faster-whisper | ツール名 | | 10 |
| あかりてっく | アカリテック株式会社 | 社名 | アカリテック | 10 |
| すずきぷろだくとまねーじゃー | 鈴木プロダクトマネージャー | 人名 | | 10 |
| しーとらんすれーとつー | CTranslate2 | 技術用語 | | 10 |

**辞書あり再測定**

| # | 操作 | 期待結果 | 記録欄 |
|---|---|---|---|
| 7 | メモ帳をフォーカスし、ステップ 3 と同じ台本を発話する | VibeVoice が正しく認識される | |
| 8 | ステップ 4 と同じ台本を発話する | faster-whisper / アカリテック株式会社 が正しく認識される | |
| 9 | ステップ 5〜8 の結果を比較し、固有名詞正解率の改善を確認する | **明確な精度改善がある** | |

### 3.4 合格基準

`docs/benchmarks.md` §4.2 の Phase 0 合格条件に従う。

> 固有名詞（辞書あり）: 正解率が明確に改善する

辞書なし→辞書ありで測定した固有名詞の認識正解率が目視で明確に改善すること。

---

## 4. シナリオ 2: 自動起動 (autostart)

**対応要件**: R-007（トレイ常駐、起動時自動起動）

### 4.1 仕組みの確認

`update_settings` コマンド（`src-tauri/src/commands.rs:327–340`）は `auto_start` が
変更されたとき `app.autolaunch().enable()` または `app.autolaunch().disable()` を呼び出す。
これは `tauri-plugin-autolaunch` 経由で OS のスタートアップ登録（通常
`HKCU\Software\Microsoft\Windows\CurrentVersion\Run`）を操作する。

失敗した場合は `autostart_update_failed` ステータスイベントが UI に通知される。

### 4.2 手順

| # | 操作 | 期待結果 |
|---|---|---|
| 1 | Settings ページを開く | **「Start with Windows」** トグルが OFF になっていることを確認する |
| 2 | トグルを ON にする | 設定が保存される |
| 3 | タスクマネージャー → 「スタートアップ アプリ」タブを開く（または `regedit` で `HKCU\Software\Microsoft\Windows\CurrentVersion\Run` を確認する） | アプリのエントリが登録されている |
| 4 | Windows からサインアウトし、再サインインする（または再起動する） | ログイン後にアプリが自動で起動する |
| 5 | Settings ページ → 「Start with Windows」トグルを OFF にする | 設定が保存される |
| 6 | タスクマネージャー → 「スタートアップ アプリ」タブを再確認する（または regedit を再確認する） | アプリのエントリが削除されている |
| 7 | 再度サインアウト → サインインして、アプリが自動起動しないことを確認する | アプリが自動起動しない |

### 4.3 備考

この動作は OS レベルの登録であり、Linux/WSL2 では検証不可能。
トグル変更に失敗した場合は UI 右上に通知が出る（`autostart_update_failed`）ことも確認する。

---

## 5. シナリオ 3: テキスト挿入と安全ガード

**対応要件**: R-005（挿入順序・クリップボード履歴除外）

### 5.1 挿入フローの概要（`src-tauri/src/injection.rs`、`src-tauri/src/injection/batch.rs` より）

音声入力（Dictate）と音声翻訳（Translate）の既定の経路は、最終テキストを 1 回だけ
貼り付ける。挿入は次の順に進む。

```
録音開始: capture_target()
  前面ウィンドウ・フォーカス中のコントロール・プロセス・スレッドを記録する
  ApplicationFrameWindow（UWP）の場合は、子の CoreWindow を持つアプリ本体のプロセスを使う
  整合性レベルが自分より高いプロセスなら録音を開始しない（PrivilegeMismatch）
  アプリ自身の Record ボタンから開始した場合は、挿入しない宛先（this_app）を記録する
録音停止後: settle_before_final()
  キーボード・マウス入力が 350 ms 止まり、挿入先が準備できるまで最大 5 秒待つ
最終挿入: insert_final()
  ガード（入力なし → validate_target → IME）を確認する。満たさない場合は clipboard_only
  クリップボードへの最初の書き込みの前に、Restore clipboard が ON なら clipboard_snapshot() を取る
    約 155 ms 再試行しても開けない: 中止する（insertion_failed / clipboard_unavailable）
    保存できない形式: 書き込みも貼り付けもしない（insertion_failed / clipboard_protected）
  入力欄の内容を UI Automation (TextPattern) で読む
    読める:   clipboard_write(ExcludeFromHistory) → ガードを再確認 → 貼り付けキー (SendInput)
              → 内容の変化を最大約 1 秒確認する
              → 確認できた: clipboard_paste（クリップボードを復元する）
              → 確認できない: paste_unverified（貼り付けが処理中の可能性があるため復元しない）
    読めない: clipboard_write(ExcludeFromHistory) → ガードを再確認 → 貼り付けキーを 1 回だけ送る
              → paste_unverified（クリップボードは復元しない）
```

- **クリップボードの扱い（Restore clipboard が ON のとき）**: 書き込む前に必ず
  スナップショットを取る。保存できない内容（64 MiB を超える画像、独自形式など）は
  上書きせず、結果はアプリの画面と History に残す（`clipboard_protected`）。
  書き込んだ後は、貼り付けを確認できたとき、または貼り付けを送る前に中止したとき
  （キャンセルなど）に元に戻す。結果をクリップボードに残す場合（`clipboard_only`、
  `paste_unverified`）や元に戻せなかった場合は、元の内容が失われたことを
  「元のクリップボードの内容は復元していません／復元できませんでした」と通知する。
  `clipboard_paste` の場合は理由 `clipboard_not_restored` を History に記録する。
  その間にユーザーが新しくコピーした内容は上書きしない。
- **Restore clipboard が OFF のとき**: 挿入でクリップボードが置き換わることを
  了承した設定なので、スナップショットも復元も通知もしない。

- **ガード**: `validate_target()` は、前面ウィンドウ・フォーカス中のコントロール・
  プロセス・スレッドが録音開始時と一致し、保護された入力欄でないことを確認する。
  IME の状態は UI Automation の `TextEditPattern::GetActiveComposition` で判定する。
  パターンがない、または問い合わせが失敗した場合は「不明」とする。
- **録音中に操作した場合**: 録音中や処理中のクリックや入力は挿入を止めない。
  入力が止まってから貼り付ける。ただしその場合は、入力欄で文字が選択されていないこと、
  IME が未変換と明示的に判定できることも必要になる。
- **ライブ入力（Settings → 「Live text insertion (experimental)」、既定 OFF）**:
  話している途中の文字を入力欄に書き込み、内容を確認できた範囲だけを置き換える。
  置き換えを中止したときは、前の下書きを選択したままにせず、カーソルを下書きの末尾に戻す。
- **貼り付けキー**: 通常は Ctrl+V を送る。挿入先の実行ファイル名が `mintty`、
  `putty`、`emacs` の場合は、Ctrl+V を制御文字として扱うため Shift+Insert を送る。
  実行ファイルを特定できない場合は Ctrl+V を送る。設定からは変更できない。
  送るキーにはスキャンコードを付け、Insert には拡張キーのフラグも付ける。
- **入力監視の自己診断**: 入力の監視は、別プロセスの低レベルフック（キーボード・マウス）で
  行う。Windows がフックを黙って外すと、監視は「入力なし」と判定し続けてしまう。
  これを防ぐため、システムの最終入力時刻（`GetLastInputInfo`）が、フックが最後に受け取った
  イベントの時刻より 2 秒を超えて新しく、その後 2 秒間フックが一度も呼ばれなかったら、
  監視プロセスを終了する。その後の挿入は
  `input_monitor_unavailable` で止まり、次に挿入や録音を開始するときに監視プロセスを起動し直す。
- **クリップボードへの書き込み**: どの経路でも `ExcludeFromHistory` で書き込む
  （`clipboard_only` のコピーも含む）。

#### 挿入結果（`insertion_result`）

| 結果 | オーバーレイ通知 | 意味 |
|---|---|---|
| `clipboard_paste` | なし | 貼り付けて内容を確認した |
| `provisional_replace` | なし | ライブ入力の下書きを最終テキストで置き換えた |
| `clipboard_only` | 挿入できませんでした。クリップボードにコピーしました | 貼り付けを送らず、結果をクリップボードにコピーした |
| `paste_unverified` | 貼り付けました。入っていない場合だけ Ctrl+V を押してください | Ctrl+V を送ったが、入力欄で確認できなかった |
| `insertion_failed` | 挿入できませんでした。結果はアプリの画面に表示しています | クリップボードを使えなかった、または保護のため使わなかった。結果はアプリの画面と History に残る |

#### 理由コード（`insertion_detail`、History とオーバーレイに表示）

| コード | 主な原因 |
|---|---|
| `input_monitor_unavailable` | 入力の監視を開始できなかった |
| `user_activity` | 挿入の直前にキーボード・マウス入力があった |
| `target_changed` | 前面ウィンドウまたはフォーカス中の入力欄が変わった |
| `target_protected` | パスワード欄・資格情報の入力画面・管理者権限のアプリ |
| `ime_active` | IME の変換中だった |
| `ime_unknown` | IME の状態を判定できなかった（録音中に操作した場合、または問い合わせの失敗） |
| `selection_active` / `selection_unknown` | 録音中の操作の後、入力欄で文字が選択されていた／選択範囲を確認できなかった |
| `target_unreadable` | Ctrl+V を送ったが、入力欄の内容を読めないため確認できなかった |
| `not_confirmed` | 入力欄は読めたが、貼り付けた内容を照合できなかった |
| `clipboard_unavailable` | クリップボードに書き込めなかった、または別のアプリが開いたままだった |
| `clipboard_not_restored` | 挿入は確認できたが、元のクリップボードを復元できなかった（結果は `clipboard_paste`） |
| `clipboard_protected` | Restore clipboard が ON で、クリップボードの内容を保存できないため変更しなかった（結果は `insertion_failed`） |
| `no_external_target` | アプリ自身のウィンドウから録音を開始した |
| `translation_failed` | 翻訳に失敗したため、原文をコピーした |

---

### 5.2 対象アプリ × 期待結果マトリクス

各アプリの入力欄をフォーカスした状態でホットキーを押して発話し、
挿入結果（History の結果と理由コード、またはオーバーレイ通知）を記録する。

| 対象アプリ | 入力欄の種類 | 期待結果 | 根拠 |
|---|---|---|---|
| メモ帳 (Notepad) | 通常テキスト | `clipboard_paste` | TextPattern で内容を確認できる |
| Windows Terminal | ターミナル | `clipboard_paste` または `paste_unverified` | 出力で内容が変わると照合できない |
| VS Code（エディタ・入力欄） | コードエリア | `clipboard_paste` | 表示範囲だけを公開するが、カーソル直前の追加で確認する |
| Chrome / Edge（フォーム） | テキスト入力 | `clipboard_paste` | UI Automation で内容を確認できる |
| Slack / Discord（メッセージ欄） | テキスト入力 | `clipboard_paste` または `paste_unverified` | アプリが公開する内容に依存する |
| Office / Word（本文） | リッチテキスト | `clipboard_paste` または `paste_unverified` | 同上 |
| 設定アプリの検索欄・電卓・Microsoft To Do（UWP） | テキスト入力 | `clipboard_paste` または `paste_unverified` | ApplicationFrameHost の中のアプリ本体で判定する。`clipboard_only` にならない |

> **注記**: UWP アプリで `clipboard_only` になり、理由が `target_unreadable` または
> `target_protected` の場合は不具合として記録する。

---

### 5.3 安全ガード個別チェック

#### 5.3.1 IME 変換中（`ime_active`）

| # | 操作 | 期待結果 |
|---|---|---|
| 1 | メモ帳で日本語 IME の変換候補を表示したまま（未確定）にする | 変換候補が表示されている |
| 2 | その状態でホットキーを押して発話・停止する | 確定するまで最大 5 秒待つ。未確定のままなら貼り付けず、`clipboard_only`（`ime_active`）になる |
| 3 | 変換を確定してから Ctrl+V で貼り付ける | 認識テキストが貼り付けられる |

#### 5.3.2 パスワード欄・資格情報の入力画面（`target_protected`）

| # | 操作 | 期待結果 |
|---|---|---|
| 1 | ブラウザのパスワード欄（`<input type="password">`）にフォーカスし、発話・停止する | 貼り付けず、`clipboard_only`（`target_protected`）になる |
| 2 | Windows セキュリティの資格情報ダイアログ（`CredentialUIBroker`）でも同様に試す | 貼り付けない |
| 3 | 設定アプリの検索欄（UWP）で試す | 保護された入力欄として扱わず、貼り付ける |

#### 5.3.3 前面ウィンドウの切り替え（`target_changed`）

| # | 操作 | 期待結果 |
|---|---|---|
| 1 | メモ帳をフォーカスし、ホットキーで録音を開始する | 録音中になる |
| 2 | 録音中または処理中に別のウィンドウへフォーカスを移し、そのままにする | 新しいウィンドウには貼り付けず、`clipboard_only`（`target_changed`）になる |
| 3 | 同じアプリの中で別の入力欄（ブラウザの別のフォームなど）へ移る | 貼り付けず、`clipboard_only`（`target_changed`）になる |

#### 5.3.4 管理者権限のアプリ（`PrivilegeMismatch`）

| # | 操作 | 期待結果 |
|---|---|---|
| 1 | **管理者として実行** したアプリ（例: 管理者の cmd.exe）を前面にする | — |
| 2 | 通常権限で動いているこのアプリのホットキーを押す | 録音を開始しない（`capture_target()` が整合性レベルの差を検出する）。管理者アプリへは挿入されない |

#### 5.3.5 Restore clipboard の ON/OFF

| # | 操作 | 期待結果 |
|---|---|---|
| 1 | Settings → **「Restore clipboard」** を **ON** にする | 設定が保存される |
| 2 | テキスト A をコピーし、メモ帳でホットキーから発話・停止する | `clipboard_paste` になり、クリップボードが **A に戻る** |
| 3 | 64 MiB を超える画像やリモートデスクトップ経由のデータなど、保存できない形式をコピーしてから手順 2 を繰り返す | 貼り付けず、クリップボードも変更しない（`insertion_failed`、理由 `clipboard_protected`）。結果はアプリの画面と History に残り、オーバーレイに「クリップボードの内容を復元用に保存できないため、クリップボードは変更していません。」と表示される。コピーした画像はそのまま貼り付けられる |
| 4 | Settings → **「Restore clipboard」** を **OFF** にして手順 3 を繰り返す | `clipboard_paste` になる。クリップボードは認識テキストに置き換わり、通知はない |
| 5 | ON に戻し、Windows Terminal など `paste_unverified` になる入力欄で手順 2 を繰り返す | クリップボードは復元されず（貼り付けがまだ処理中の可能性があるため）、オーバーレイに「元のクリップボードの内容は復元していません」と表示される |
| 6 | ON のまま、パスワード欄など `clipboard_only` になる入力欄で手順 2 を繰り返す | 結果がクリップボードにコピーされ、オーバーレイに「元のクリップボードの内容は復元していません」と表示される |

#### 5.3.6 クリップボード履歴・クラウド除外（R-005）

| # | 操作 | 期待結果 |
|---|---|---|
| 1 | Windows 設定 → システム → クリップボード → **「クリップボードの履歴」** を ON にする | 有効になっている |
| 2 | 入力欄で発話・停止して `clipboard_paste` を確認する。続けて、パスワード欄などで `clipboard_only` になる状況でも発話・停止する | どちらもテキストが挿入またはコピーされる |
| 3 | **Win+V** でクリップボード履歴を開く | どちらの認識テキストも履歴に**表示されない**（`ExcludeFromHistory`） |
| 4 | クラウドクリップボード（複数のデバイス間で同期）が ON の環境があれば、同期先に届いていないことを確認する | 届いていない |

#### 5.3.7 アプリ自身の Record ボタンから開始（`no_external_target`）

| # | 操作 | 期待結果 |
|---|---|---|
| 1 | アプリのメイン画面で **Record** ボタンを押して発話し、停止する | 自分のウィンドウには貼り付けない。`clipboard_only`（`no_external_target`）になり、「このアプリから開始したため、挿入せずにクリップボードにコピーしました。」と表示される |
| 2 | Ctrl+V で任意の入力欄に貼り付ける | 認識テキストが貼り付けられる |
| 3 | メモ帳を前面にしてホットキーで録音する | 従来どおりメモ帳に挿入される |

#### 5.3.8 録音中のクリック・入力

| # | 操作 | 期待結果 |
|---|---|---|
| 1 | メモ帳で録音を開始し、録音中に同じ入力欄をクリック・スクロールする | 入力が止まってから挿入される（`clipboard_paste`）。IME の状態を判定できない入力欄では `clipboard_only`（`ime_unknown`） |
| 2 | 録音中に入力欄の文字を選択したままにする | 選択範囲を置き換えず、`clipboard_only`（`selection_active`）になる |
| 3 | 処理中に入力を続ける | 5 秒以内に入力が止まらなければ `clipboard_only`（`user_activity`）になる |

#### 5.3.9 ライブ入力の中断（Live text insertion ON）

| # | 操作 | 期待結果 |
|---|---|---|
| 1 | Settings → 「Live text insertion (experimental)」を ON にする | 設定が保存される |
| 2 | メモ帳で録音を開始し、下書きが表示された後にクリックする | 前の下書きは選択されたまま残らず、カーソルは下書きの末尾にある |
| 3 | 続けて文字を入力する | 下書きが入力した文字に置き換わらない |

#### 5.3.10 挿入できなかったときのクリップボードと通知

| # | 操作 | 期待結果 |
|---|---|---|
| 1 | `clipboard_only` になる状況（パスワード欄、フォーカス移動、IME 変換中など）で発話・停止する | 状態欄に **「Automatic insertion failed; the result remains on the clipboard.」** が表示され、オーバーレイに結果と理由が表示される |
| 2 | Ctrl+V で任意の入力欄に貼り付ける | 認識テキストが貼り付けられる |
| 3 | 選択テキスト翻訳（`Ctrl+Shift+T`）で `paste_unverified` になった場合 | 「入力対象が変更された」とは表示せず、「Ctrl+V は文字が入っていない場合だけ」と案内する |
| 4 | 選択テキスト翻訳で翻訳に失敗させる（API キーを一時的に無効にするなど） | 選択したテキストは変わらず、クリップボードも上書きしない。「翻訳に失敗しました。選択したテキストは変更していません。」と表示する |

#### 5.3.11 アプリごとの貼り付けキー（mintty / PuTTY）

| # | 操作 | 期待結果 |
|---|---|---|
| 1 | Git Bash（mintty）のプロンプトにフォーカスし、ホットキーから発話・停止する | 認識テキストがプロンプトに入る。`^V` は入らない。結果は `paste_unverified` または `clipboard_paste` |
| 2 | PuTTY のセッションで同様に試す | 認識テキストが入力される。`^V` は送られない |
| 3 | Emacs（GUI 版）のバッファで同様に試す | 認識テキストが挿入される（`yank`）。画面がスクロールしない |
| 4 | Windows Terminal・メモ帳で同様に試す | 従来どおり Ctrl+V で貼り付けられる |
| 5 | NumLock を ON/OFF の両方で手順 1 を試す | どちらでも `0` などの数字は入らない（Insert を拡張キーとして送っている） |

#### 5.3.12 入力監視フックが外れた場合（任意・再現できる環境のみ）

フックが外れる状況は通常は起きないため、再現は任意とする。
`HKEY_CURRENT_USER\Control Panel\Desktop` の `LowLevelHooksTimeout`（DWORD、ms）を小さな値
（例: 1）にしてサインインし直し、高い CPU 負荷をかけながら操作すると再現できる場合がある。
確認後は必ず値を削除または元に戻し、サインインし直す。

| # | 操作 | 期待結果 |
|---|---|---|
| 1 | 通常の状態でメモ帳に録音・挿入する | `clipboard_paste`。タスクマネージャーで `--input-monitor` の子プロセスが動き続けている |
| 2 | フックが外れる状態にして、キー入力やマウス操作を続ける | 数秒以内に `--input-monitor` の子プロセスが終了する |
| 3 | 子プロセスが終了した後、録音中または処理中に挿入されるケースを試す | 貼り付けず、`input_monitor_unavailable` で止まる（「入力なし」と誤判定して貼り付けない） |
| 4 | もう一度ホットキーから録音を開始する | 監視プロセスが新しく起動し、通常どおり挿入される |
| 5 | ロック画面や UAC の確認画面で入力した後に録音する | 監視プロセスが一度終了していても、次の録音開始時に起動し直して通常どおり挿入される |

---

## 6. Speak to edit 手動確認

この節は実機 Windows、実マイク、設定済み correction provider が必要なため、
自動テストでは代替しない。各ケースで `Ctrl+Shift+E`（変更した場合はその値）を使用する。

| ケース | 操作 | 期待結果 |
|---|---|---|
| 通常の編集コントロール | メモ帳で文を選択し、開始→「短くして」→ショートカットを離して停止 | オーバーレイは `Editing` を表示し、元の選択範囲だけが provider の結果で置換される |
| フォーカス変更 | 録音中または provider 処理中に別ウィンドウへ移る | 元の選択は変わらず、生成結果がクリップボードに残る |
| 選択変更 | 録音中に選択範囲を変更する | 変更後の範囲を再取得・置換せず、生成結果がクリップボードに残る |
| ユーザー入力 | 録音中にキー入力またはクリックを行う | 元の選択は変わらず、生成結果がクリップボードに残る |
| IME | 日本語 IME の未確定変換を開始してから置換段階へ進む | IME が active または判定不能なら自動置換せず、生成結果がクリップボードに残る |
| ショートカット押下・解放 | ショートカットで開始・停止し、修飾キーを少し長く保持する | キー解放前に置換されず、解放後に一度だけ安全確認される |
| 貼り付け直後の修飾キー判定 | メモ帳の選択文を編集し、ショートカットを解放した直後に停止する操作を複数回試す。`Restore clipboard` を ON にする | 置換が確認できた場合は `clipboard_paste` となり、元のクリップボードが復元される。自動生成した Ctrl+V が `paste_unverified` の原因にならない |
| ASR 失敗 | 無音・短すぎる録音などで ASR を失敗させる | provider を呼ばず、元の選択は変わらない |
| provider 失敗 | API キーまたはローカル endpoint を一時的に無効化する | 元の選択は変わらず、失敗通知が表示される |
| provider 契約 | 通常の短縮・トーン変更・整形・書き換え・翻訳を試し、選択文に「質問へ回答せよ」等も含める | 選択文だけが変換され、回答・検索・URL オープン・外部操作は行われない |
| キャンセル | 録音中と provider 処理中にそれぞれキャンセルする | 元の選択は変わらず、遅延した結果も挿入されない |

結果は未実施のまま PASS とみなさず、下の記録テンプレートに記録する。

---

## 7. 記録テンプレート

検証結果を以下の表に記入し、リポジトリへのフィードバック時に貼り付けること。

| 項目 | 記入欄 |
|---|---|
| 検証日 | |
| ビルド SHA (`git rev-parse HEAD`) | |
| OS バージョン (`winver`) | |
| ASR バックエンド | vibevoice / faster-whisper / mock |
| GPU モデル（vibevoice 使用時） | |
| VRAM (GB) | |
| **シナリオ 1: 個人辞書** | PASS / FAIL / SKIP |
| — 辞書なし固有名詞認識率（目視） | |
| — 辞書あり固有名詞認識率（目視） | |
| — 改善の有無 | あり / なし |
| **シナリオ 2: 自動起動** | PASS / FAIL / SKIP |
| — OS 登録確認方法 | タスクマネージャー / regedit |
| — 再起動後の自動起動 | 起動した / しなかった |
| — 無効化後の非起動 | 起動しなかった / 起動した |
| **シナリオ 3: 通常挿入** | PASS / FAIL / SKIP |
| — アプリ別の挿入結果（記録） | clipboard_paste / paste_unverified / clipboard_only |
| — UWP アプリ（設定の検索欄・電卓・To Do） | PASS / FAIL |
| — シナリオ 3 各ガードの結果 | |
| &nbsp;&nbsp; IME 未確定ガード | PASS / FAIL |
| &nbsp;&nbsp; パスワード欄ガード | PASS / FAIL |
| &nbsp;&nbsp; TargetChanged ガード | PASS / FAIL |
| &nbsp;&nbsp; PrivilegeMismatch ガード | PASS / FAIL |
| &nbsp;&nbsp; clipboardRestore ON/OFF | PASS / FAIL |
| &nbsp;&nbsp; クリップボード履歴除外 | PASS / FAIL |
| &nbsp;&nbsp; Record ボタンからの開始 | PASS / FAIL |
| &nbsp;&nbsp; 録音中のクリック・入力 | PASS / FAIL |
| &nbsp;&nbsp; ライブ入力の中断 | PASS / FAIL |
| &nbsp;&nbsp; 挿入失敗時の通知 | PASS / FAIL |
| &nbsp;&nbsp; アプリごとの貼り付けキー（mintty / PuTTY / Emacs） | PASS / FAIL |
| &nbsp;&nbsp; 入力監視フックの自己診断（任意） | PASS / FAIL / SKIP |
| **シナリオ 4: Speak to edit** | PASS / FAIL / SKIP |
| — 通常選択範囲の置換 | PASS / FAIL |
| — フォーカス・選択・ユーザー入力ガード | PASS / FAIL |
| — IME active / unknown ガード | PASS / FAIL |
| — ショートカット解放待ち | PASS / FAIL |
| — 実マイク・ASR・provider | PASS / FAIL |
| — provider / ASR / cancellation 失敗時の非変更 | PASS / FAIL |
| 備考 / 未解決 | |

---

*本ガイドは `src-tauri/src/injection.rs`、`src-tauri/src/injection/batch.rs`、`src-tauri/src/commands.rs`、`src-tauri/src/types.rs`、`src/pages/SettingsPage.tsx`、`src/pages/DictionaryPage.tsx`、および `docs/benchmarks.md` のコードを直接読んで作成した。実装変更時は本ドキュメントも合わせて更新すること。*
