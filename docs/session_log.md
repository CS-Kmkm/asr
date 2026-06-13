# セッションログ — ローカル音声入力アプリ品質改善

最終更新: 2026-06-13

## 体制

メインのClaudeが**指揮役**(`/orchestrate` スキル)として、実装をサブエージェント(Codex / Opus 4.8 / Sonnet 4.6)へ委譲し、各成果を `git diff` 確認とテスト再実行で検収する運用。

検証環境はWSL2のため **Windows固有機能・CUDA・`cargo check` は実行不可**。Rustコンパイル検証はWindows実機でユーザーが行う前提。`cargo fmt --check` と `npm run build`(tsc)とPython `pytest` はこの環境で実行可能。

## 完了・検収合格済み(Wave 1 + 2)

| 成果物 | 担当 | 内容 | 検収 |
|---|---|---|---|
| `docs/local_ai_voice_input_implementation_plan.md` 全面改訂 | Sonnet | PySide6前提→Tauri+Rust+React+Pythonサイドカーの実装に追従。短文KPI追加、クリップボード除外・IMEガード・マイグレーション方針を要件化 | Cargo.tomlと一致確認 |
| `docs/adr/` ×3 | Sonnet | 技術スタック / 一括認識v1 / 配布方式(未決定・案②暫定推奨) | 合格 |
| `docs/benchmarks.md` + `scripts/bench_asr.py` | Opus | KPI予算分解、候補モデル比較、計測ハーネス(cold/warm分離) | smoke成功 |
| faster-whisperバックエンド | Opus | `asr_worker/backends.py` にCPU可バックエンド追加。VibeVoiceに `max_new_tokens` 上限。CUDA必須の制約が外れた | 27テストパス |
| `README.md` 更新 | Sonnet | バックエンド選択手順、GPU要件緩和 | 合格(見出し1件微修正) |
| `docs/worker_protocol.md` | Opus | JSONLプロトコルv1仕様化、cancel設計=kill+再spawn推奨 | 全14エラーコード一致 |
| `scripts/score_asr.py` | Opus | CER/WER/固有名詞正解率の採点 | self-test全PASS |
| `docs/eval_set/` | Sonnet | 日英・混在の固定台本27本+正解+用語18語 | score_asrで読込確認 |

**到達点:** Windows実機でPhase 0計測がエンドツーエンドで実行可能(録音→`bench_asr.py`→`score_asr.py`→`benchmarks.md` 記入)。

## Wave 3.1(完了・検収合格)

- **3.1-A ASRバックエンド選択(Rust+UI設定永続化)**: Opus。`Settings.asr_backend` 追加(デフォルトvibevoice、camelCase `asrBackend`)、UIセレクト、ワーカー起動引数 `--backend`、変更時に `reconfigure` でワーカーリセット+quantizationクリア。検収: `cargo fmt --check` 差分なし / `npm run build` 成功 / pytest 27件 / serde命名・レガシーフォールバック確認。
- **3.1-B クリップボード履歴除外+IMEガード**: Opus。一時クリップボード経路に `ExcludeClipboardContentFromMonitorProcessing` / `CanIncludeInClipboardHistory`=0 / `CanUploadToCloudClipboard`=0 をベストエフォートで付与(終端のclipboard-only保持は履歴許可)。IMEガードは `ImmGetCompositionStringW(GCS_COMPSTR)` 長>0で挿入中止→クリップボード保持。孤立していた `ImeCompositionActive` バリアントを解消。検収: `cargo fmt --check` 差分なし、孤立解消確認。

### Windows実機で要確認(3.1)
- `cargo check` / `cargo test`(WSL2では不可。新規Rustテストも実機実行)。
- 3.1-A: UIでバックエンド切替→次回ロードで新workerが起動。切替直後にModelStatus "ready" 表示が残る軽微な既知事項(再ロードで解消)。
- 3.1-B: ① `ExcludeClipboardContentFromMonitorProcessing` のペイロード仕様(現状1バイトマーカ)が効くか — Win+Vに転写が出ないこと、別デバイスのクラウドクリップボードに同期されないことを確認。効かない場合はDWORD形式へ差し替え可。② 日本語IME変換中に挿入されず、未確定文字列が破壊されないこと。`windows` 0.58 のImm系シグネチャ/`GCS_COMPSTR` パスの確認。

## Wave 3.2(完了・検収合格)

- **プリアーム+プリロールバッファ録音**: Opus。`audio.rs` に固定容量リング `PrerollBuffer`(oldest-first、drain/snapshot、容量0で無効=従来動作)を追加。`CaptureConfig.preroll`(デフォルト300ms)、`AudioCapture` traitに `arm()`/`disarm()` 追加。`start()` はアーム済みストリームを録音モードへ遷移させ、`begin_recording()` でプリロール内容を録音バッファ先頭へ継ぎ足し(発話先頭の欠落ゼロ)。リングは入力デバイスのネイティブレートで保持し、`finalize_samples` で本体と一括リサンプルするため継ぎ目のレート不整合なし。cpalコールバックは既存の単一ロック設計を維持。
- **lib.rs配線**: `start_recording` で `arm→start`(armは冪等、初回コールドpressも吸収)、`stop_recording` 後にベストエフォートで再arm(次回pressをwarm化)。失敗時はコールドスタートにフォールバック。`setup()` では事前armしない(起動直後・cancel後はマイク閉。プライバシー保守的)。
- **検収**: `cargo fmt --check` 差分なし。crate全体の `cargo test` はwebkit2gtk未導入でLinux不可のため、純粋ロジック(PrerollBuffer + duration_to_samples + begin_recording)を抽出し `rustc --test` で独立検証 → **8テスト全パス**(容量0無効/部分窓順序/最古退避/容量超過なし/チャンク跨ぎwrap/duration×rateサイズ/プリロール継ぎ足し冪等/プリロールなし空開始)。lib.rs配線も目視確認。

### Windows実機で要確認(3.2)

- `cargo test`(cpalゲート経路含むフル実行)。
- 発話先頭が切れないこと: `delete_audio_after_processing=false` でWAVを保存し、停止→即再pressで発話冒頭の子音/音素が先頭~300msに残ることを旧ビルドと比較。
- プライバシー論点: 停止後は再armでマイクが開いたまま(次press高速化)。「アイドル時にマイクを閉じる」設定を将来公開する場合は実装済みの `disarm()` をstop後に呼ぶ。`preroll=0` で完全に従来動作へ戻せる。

## Wave 3 後の cargo check 通過(WSL2・rootなし)

WSL2でTauri crateの `cargo check` を通すため、root権限なしで以下を実施:

- 不足していた `webkit2gtk-4.1` / `javascriptcoregtk-4.1` / `libsoup-3.0`(システムには4.0/2.4のみ存在)の dev/lib debを `apt-get download`(root不要)で取得し、`$HOME/.local/tauri-deps` に `dpkg -x` で展開。`.pc` の `prefix=/usr` をローカルprefixへ書き換え、`Requires.private`(静的リンク専用)を除去、`sysprof-capture-4` はスタブ `.pc` で解決。`PKG_CONFIG_PATH` にローカルprefixを前置して `cargo check` 実行。
- この `cargo check`(ネイティブLinuxターゲット)で **実コンパイルエラー2件を検出・修正**:
  - `Cargo.toml`: tokio features に `macros` が不足し `tokio::select!`(asr.rs:251、キャンセル処理)がコンパイル不能 → `macros` 追加。
  - `asr.rs`: `AsyncReadExt` のimport漏れで `(&mut stdout).take(...)`(asr.rs:165)が `Iterator::take` 扱いになりエラー → import追加。
  - いずれもプラットフォーム非依存の必須修正(Windowsビルドでも同じく失敗していたはずの既存バグ)。指揮役のglueとして直接修正。
- 環境要因(恒久対応済み): Linuxの `generate_context!` が `icons/icon.png` を要求していた(従来 tauri.conf.json は `icon.ico` 1枚=16×16のみ参照で、Windows製品としても不十分)。ユーザー提供のロゴ `src-tauri/icons/logo.png`(1254×1254)から `npx tauri icon` で全プラットフォームのアイコン一式(icon.png / 32x32 / 128x128 / 128x128@2x / icon.icns / 複数サイズ icon.ico / Square*Logo 等)を生成し、`bundle.icon` を標準構成へ更新。これで**一時pngなしで Windows実ビルドも Linux の `cargo check` も両方通る**。
- **結果: `cargo check` がエラー0で通過**(警告11件はすべて `#[cfg(windows)]` 側でのみ使われるコードがLinuxチェックで未使用になる想定内のもの)。`cargo fmt --check` 差分なし、pytest 27件パスも維持。

### この cargo check の検証範囲と限界
- **検証済み**: プラットフォーム非依存の全Rust(3.1-A バックエンド選択の全体、lib.rs配線、asr.rs、storage.rs、types.rs、3.2 の `PrerollBuffer`、injection.rs の orchestrate ロジックと非windowsバックエンド)。
- **未検証(Windows実機必須)**: `#[cfg(target_os = "windows")]` 配下(audio.rs の cpalコールバック、injection.rs の `windows_backend`=クリップボード除外/IMEガードの実API呼び出し)。`cargo check --target x86_64-pc-windows-msvc` はMSVC Cツールチェーン(lib.exe)が必要でこの環境では不可。

## 環境整備済み

- バックグラウンドエージェントのEdit/Write許可: `~/.claude/settings.json` に `Edit/Write(//home/koshi/asr/**)`、`worktree.bgIsolation: "none"`、`npm`/`cargo fmt`/`node` のBash許可を追加済み。
- プロジェクト側 `.claude/settings.json` にも同等のallowを設定済み(次回セッションから有効)。
- Rust検証の限界: crate全体はtauri経由のwebkit2gtk/libsoup依存でLinuxビルド不可。プラットフォーム非依存ロジックは `rustc --test` での抽出検証で代替。フル `cargo check`/`cargo test` はWindows実機、または `sudo apt-get install -y libwebkit2gtk-4.1-dev libsoup-3.0-dev build-essential pkg-config` 導入後にWSLで可能。

## 残タスク(改善計画の対応)

1. **Windows実機検証**(最優先): Wave 3.1/3.2のRustコンパイル(`cargo check`/`cargo test`)、UI動作、上記の各実機確認項目。
2. **Phase 0計測の実行**: `docs/eval_set/` の台本を録音 → `bench_asr.py` → `score_asr.py` → `benchmarks.md` のゲート判定表に記入。日常入力デフォルトプロバイダーを決定。
3. **配布方式の決定**(ADR-0003、Step 4): Phase 0実測が材料。
4. **Wave 3.3(任意)**: ウォームアップ推論、録音中チャンク先行転写(Phase 1.5)。

## 改善計画6ステップの進捗

- Step 1(ドキュメント正準化+ADR): 完了
- Step 2(ベンチ基盤・短文KPI): 基盤完了、実測待ち
- Step 3(軽量ASR): 完了
- Step 4(配布方式決定): 未着手(実測待ち)
- Step 5(クリップボード除外・IMEガード・cancel): 完了(cancel設計文書化 + Rust実装はWave 3.1)
- Step 6(プリアーム・プリロール): 完了(Wave 3.2)。ウォームアップ/チャンク先行転写は任意の追加課題
