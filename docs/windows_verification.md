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
| IME ガード | `ImmGetCompositionStringW` |
| セキュア入力検出 | `ES_PASSWORD` スタイル / UIAutomation `IsPassword` / `CredentialUIBroker` 等 |
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
npm install
py -3.10 -m venv .venv
.\.venv\Scripts\Activate.ps1
pip install -e .                          # デフォルト (faster-whisper、CPU対応・軽量)
# pip install -e ".[vibevoice]"           # VibeVoice バックエンドを使う場合 (GPU必須)
$env:ASR_PYTHON = "$PWD\.venv\Scripts\python.exe"

# 起動
npm run tauri dev

# mock バックエンドでプロトコル確認のみ (モデル不要)
$env:ASR_WORKER_BACKEND = "mock"
$env:ASR_WORKER_MOCK_TEXT = "テスト認識テキスト"
npm run tauri dev
```

---

## 2. 共通セットアップ（全シナリオ共通、1 回だけ実施）

以下の手順を順に完了させてから各シナリオへ進む。

| # | 操作 | 期待結果 |
|---|---|---|
| 1 | リポジトリを Windows 実機へ clone し、上記ビルドコマンドを実行する（`pip install -e .` でデフォルトの faster-whisper スタックがインストールされる） | `npm run tauri dev` でアプリウィンドウが開く |
| 2 | バックエンドを確認・選択する。デフォルトは **faster-whisper (CPU対応)**。GPU ありで長文/高精度用途なら Settings → ASR backend で **VibeVoice (GPU)** を選ぶ（事前に `pip install -e ".[vibevoice]"` が必要）。GPU なしで検証のみなら **mock (開発用)** を選ぶ | Settings 画面に選択が反映される |
| 3 | ASR モデルを読み込む。faster-whisper の場合: **Load ASR model** ボタンを押す（または初回 transcribe 時に自動ダウンロード）。VibeVoice の場合: Model & GPU ページ → **Load 4-bit model** ボタン | Model ステータスが `ready` になる |
| 4 | Setup ページ → マイクをプルダウンで選択（「System default」でも可） | マイクが設定される |
| 5 | Setup ページ → ホットキーを確認（デフォルト `Ctrl+Shift+Space`）。変更する場合は入力欄を書き換える | ホットキーが再登録される |
| 6 | メモ帳などのテキスト入力欄をフォーカスしてホットキーを押し、短文を発話してもう一度ホットキーを押す | テキストが挿入される、またはクリップボードに残る（mock の場合は `ASR_WORKER_MOCK_TEXT` の内容） |

---

## 3. シナリオ 1: 個人辞書の効果

**対応要件**: R-014（ユーザー辞書）、R-011/R-004（辞書語を ASR プロンプトへ渡す）

### 3.1 仕組みの確認

`stop_recording` コマンド（`src-tauri/src/commands.rs:125–132`）は、録音停止時に
`storage.dictionary_prompt_terms()` を呼び出して辞書の **surface + aliases** を
全件収集し、改行区切りで ASR ワーカーへ `prompt` パラメータとして送信する。
（`src-tauri/src/storage.rs:270–277`）

- **VibeVoice バックエンド**: この `prompt` が generation context として渡される。
- **faster-whisper バックエンド**: `initial_prompt` / ホットワードとして渡される。

どちらも同じ term リストを消費する。

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

### 5.1 挿入フローの概要（`src-tauri/src/injection.rs` より）

挿入は `orchestrate_insert` 関数が制御する。
フォールバック順は以下の通り（実装と 1:1 で対応）:

```
validate_target() → IME ガード → ui_automation_insert()
  → clipboard_snapshot() → clipboard_write(ExcludeFromHistory) → paste()
  → unicode_input()
  → clipboard_only_fallback(AllowHistory)
```

#### 挿入結果ラベル（UI に表示される）

| `InsertResult` | UI ラベル | 意味 |
|---|---|---|
| `UiAutomation` | `ui_automation` | UI Automation で直接挿入 |
| `ClipboardPaste` | `clipboard_paste` | クリップボード経由で貼り付け |
| `UnicodeInput` | `unicode_input` | `SendInput` Unicode イベントで挿入 |
| `ClipboardOnly` | `clipboard_only` | 自動挿入失敗、クリップボードに保持 |

> **注記**: `ui_automation_insert` は現在の Windows バックエンドで常に `false` を返す
> （`injection.rs:787–796` のコメント: ValuePattern/TextPattern サポートはアプリごとの
> 信頼性確立後に有効化予定）。したがって現時点の実機では `UiAutomation` パスは
> 通常使用されず、`ClipboardPaste` または `UnicodeInput` が主経路となる。
> テック リードは将来 UIA が有効化された際にここを更新すること。

---

### 5.2 対象アプリ × 期待挿入方式マトリクス

各アプリのテキスト入力欄をフォーカスした状態でホットキーを押して発話し、
挿入方式と結果テキストを記録する。

| 対象アプリ | 入力欄の種類 | 期待挿入方式 | 根拠 |
|---|---|---|---|
| メモ帳 (Notepad) | 通常テキスト | `clipboard_paste` または `unicode_input` | 通常フォールバック経路 |
| Windows Terminal | ターミナル | `clipboard_paste` または `unicode_input` | 同上 |
| VS Code（コードエディタ） | コードエリア | `clipboard_paste` または `unicode_input` | 同上 |
| Chrome（URL バー・フォーム） | テキスト入力 | `clipboard_paste` または `unicode_input` | 同上 |
| Edge（URL バー・フォーム） | テキスト入力 | `clipboard_paste` または `unicode_input` | 同上 |
| Slack（メッセージ欄） | テキスト入力 | `clipboard_paste` または `unicode_input` | 同上 |
| Discord（メッセージ欄） | テキスト入力 | `clipboard_paste` または `unicode_input` | 同上 |
| Office / Word（本文） | リッチテキスト | `clipboard_paste` または `unicode_input` | 同上 |

記録方法: 挿入後にアプリ右上またはステータスバーに表示される挿入ラベル
（`ui_automation` / `clipboard_paste` / `unicode_input` / `clipboard_only`）を記録する。

---

### 5.3 安全ガード個別チェック

#### 5.3.1 IME 未確定中の挿入ガード（`ImeCompositionActive`）

| # | 操作 | 期待結果 | 根拠 |
|---|---|---|---|
| 1 | メモ帳で日本語 IME を使い、変換候補ウィンドウが出ている状態（未確定）を作る | 変換候補ウィンドウが表示されている | |
| 2 | その状態でホットキーを押して発話・停止する | テキストは自動挿入されず、**クリップボードにのみ保持**される（`clipboard_only` ラベル） | `injection.rs:187–189`: `ImeCompositionActive` のとき `clipboard_only_fallback` へ分岐 |
| 3 | 未確定文字を確定（Enter キー）してから Ctrl+V で貼り付けると認識結果が得られる | 認識テキストが貼り付けられる | |

#### 5.3.2 パスワード欄 / セキュア入力欄（`SecureTarget`）

| # | 操作 | 期待結果 | 根拠 |
|---|---|---|---|
| 1 | ブラウザのパスワード入力欄（`<input type="password">`）にフォーカスする | — | |
| 2 | ホットキーを押して発話・停止する | テキストはパスワード欄に挿入されず、**クリップボードにのみ保持**される（`clipboard_only` ラベル） | `injection.rs:649–651`: `is_secure` が `true` なら `SecureTarget` エラー → `clipboard_only_fallback` |
| 3 | Windows ログイン画面 / UAC 昇格プロンプト等の `CredentialUIBroker` ウィンドウでも同様に試みる | 挿入されない | `injection.rs:508–514`: `is_known_credential_surface` チェック |

#### 5.3.3 フォアグラウンドウィンドウの切り替え（`TargetChanged`）

| # | 操作 | 期待結果 | 根拠 |
|---|---|---|---|
| 1 | メモ帳をフォーカスしてホットキーで録音開始する | 録音中になる | |
| 2 | 録音中（または発話終了直後の処理中）に別のウィンドウをクリックしてフォーカスを移す | — | |
| 3 | アプリが挿入処理に移行したとき、**テキストは新しいウィンドウに挿入されず**、クリップボードに保持される（`clipboard_only` ラベル） | `injection.rs:649–660`: `validate_target` がウィンドウハンドル・プロセス ID 不一致を検出して `TargetChanged` → `clipboard_only_fallback` |

#### 5.3.4 管理者権限アプリへの挿入（`PrivilegeMismatch`）

| # | 操作 | 期待結果 | 根拠 |
|---|---|---|---|
| 1 | **管理者として実行** した任意のアプリ（例: 管理者 cmd.exe）をフォアグラウンドにしてフォーカスする | — | |
| 2 | アプリ（通常権限で動作中）のホットキーを押して発話・停止する | テキストは管理者アプリに挿入されず、**クリップボードにのみ保持**される（`clipboard_only` ラベル） | `injection.rs:584–593`: `reject_higher_integrity` が整合性レベル差を検出して `PrivilegeMismatch` → `clipboard_only_fallback` |

> 注記: `PrivilegeMismatch` チェックは `capture_target()` 時（録音開始時）にも実行される。
> 管理者アプリがフォアグラウンドのときにホットキーを押すと、録音が始まらない場合と、
> 開始後に挿入フェーズでブロックされる場合の両方がある（タイミング依存）。
> いずれの場合も挿入は行われない。

#### 5.3.5 クリップボード方式時の `clipboardRestore` ON/OFF 動作

| # | 操作 | 期待結果 | 根拠 |
|---|---|---|---|
| 1 | Settings → **「Restore clipboard」** トグルを **ON** にする | 設定保存 | `types.rs:32`: `clipboard_restore` フィールド |
| 2 | メモ帳に任意のテキスト A を貼り付けてクリップボードに A が残っている状態にする | Ctrl+C 等でクリップボードに A を入れる | |
| 3 | 別のテキスト入力欄をフォーカスし、ホットキーで発話・停止する | 認識テキストが挿入され（`clipboard_paste`）、その後クリップボードが **A に復元**される | `injection.rs:211–215`: 貼り付け成功後に `clipboard_restore` が `true` なら `clipboard_restore()` を呼ぶ |
| 4 | Settings → **「Restore clipboard」** トグルを **OFF** にする | 設定保存 | |
| 5 | 手順 2〜3 を繰り返す | 認識テキストが挿入され、クリップボードは**復元されない**（認識テキストまたは空になる） | `injection.rs:210–218`: `restore_clipboard = false` の場合 `clipboard_restore()` を呼ばない |

#### 5.3.6 クリップボード履歴・クラウド除外（R-005）

| # | 操作 | 期待結果 | 根拠 |
|---|---|---|---|
| 1 | Windows 設定 → システム → クリップボード → **「クリップボードの履歴」** を ON にしておく | 有効になっている | |
| 2 | テキスト入力欄をフォーカスし、ホットキーで発話・停止して `clipboard_paste` 挿入を確認する | 認識テキストが挿入される | |
| 3 | **Win+V** でクリップボード履歴を開く | 挿入に使用された認識テキストは履歴に**表示されない** | `injection.rs:206–208, 378–399`: `ExcludeFromHistory` 指定で書き込むと `ExcludeClipboardContentFromMonitorProcessing`・`CanIncludeInClipboardHistory`・`CanUploadToCloudClipboard` フォーマットが付与され、履歴・クラウドから除外される |
| 4 | クラウドクリップボード（Windows 設定 → クリップボード → 複数のデバイス間で同期）が ON の環境があれば、同期先デバイスに認識テキストが届いていないことを確認する | 届いていない | 同上 |

> 注記: `clipboard_only` 経路（自動挿入失敗時）では意図的に `AllowHistory` で書き込む。
> これはユーザーが手動で貼り付けるためにテキストを保持しておく必要があるため。
> 手動貼り付けシナリオでは Win+V に認識テキストが表示されることを確認する。

#### 5.3.7 挿入失敗時のクリップボード保持と通知

| # | 操作 | 期待結果 | 根拠 |
|---|---|---|---|
| 1 | 挿入が `clipboard_only` になる状況（パスワード欄、フォーカス移動、IME 未確定など）を作り、発話・停止する | UI に **「Automatic insertion failed; the result remains on the clipboard.」** という通知が出る | `commands.rs:238–244`: `ClipboardOnly` の場合にこのメッセージを `emit_status` で送出 |
| 2 | Ctrl+V で任意のテキスト欄に貼り付ける | 認識テキストが貼り付けられる | |

---

## 6. 記録テンプレート

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
| — 主挿入方式（記録） | clipboard_paste / unicode_input |
| — シナリオ 3 各ガードの結果 | |
| &nbsp;&nbsp; IME 未確定ガード | PASS / FAIL |
| &nbsp;&nbsp; パスワード欄ガード | PASS / FAIL |
| &nbsp;&nbsp; TargetChanged ガード | PASS / FAIL |
| &nbsp;&nbsp; PrivilegeMismatch ガード | PASS / FAIL |
| &nbsp;&nbsp; clipboardRestore ON/OFF | PASS / FAIL |
| &nbsp;&nbsp; クリップボード履歴除外 | PASS / FAIL |
| &nbsp;&nbsp; 挿入失敗時の通知 | PASS / FAIL |
| 備考 / 未解決 | |

---

*本ガイドは `src-tauri/src/injection.rs`、`src-tauri/src/commands.rs`、`src-tauri/src/types.rs`、`src/pages/SettingsPage.tsx`、`src/pages/DictionaryPage.tsx`、および `docs/benchmarks.md` のコードを直接読んで作成した。実装変更時は本ドキュメントも合わせて更新すること。*
