# ADR-0001: アプリフレームワーク技術スタックの選定

## ステータス

採用済み (2026-06-13)

## 文脈

Windows向けローカルAI音声入力OSSの初期設計では、Pythonエコシステムへの親和性から
PySide6 + pywin32 + sounddevice + PyInstaller 構成を検討していた。

その後、以下の要件が設計上の制約として明確になった。

- グローバルホットキー登録とWASAPI経由の低遅延マイクキャプチャが必要
- UIプロセスとASRワーカーを別プロセスに分離し、ワーカークラッシュ時もUIを継続させる必要がある
- Windows UI Automation (COM)、整合性レベルチェック、クリップボード操作など
  複数のWin32 APIを型安全に扱う必要がある
- インストーラーを可能な限りコンパクトに保つ必要がある
- フローティングUIのアニメーションや設定UIをWeb技術で実装したい

## 決定

**Tauri 2 + Rust + React (WebView2) + Python JSONLサイドカーワーカー** を採用する。

- メインプロセスをRust (Tauri 2) で実装し、音声キャプチャ・VAD・挿入・ストレージを担当させる
- UIはWebView2内のReactで実装し、Tauri IPCでメインプロセスと通信する
- ASR推論はPythonサイドカーワーカー (`asr_worker`) に分離し、
  stdin/stdout のJSONL (改行区切りJSON) プロトコルで通信する
- Windows APIはRustの `windows` クレート (0.58) で直接バインドする
- 音声キャプチャには `cpal` (WASAPI) を使用する

## 結果

### 採用によるメリット

- **プロセス分離**: ASRワーカーがクラッシュしても Tauriメインプロセスとサイドカーの再起動で継続できる
- **低遅延キャプチャ**: cpal + WASAPI により GIL の影響を受けずにマイクデータを取得できる
- **型安全なWin32連携**: `windows` クレートにより COM、整合性レベル、UIAを安全に呼び出せる
- **コンパクトなメインバイナリ**: RustバイナリはPythonランタイムを含まず小さい
- **Web UI**: ReactによりフローティングUIのアニメーションや設定画面を実装しやすい

### トレードオフ・残存課題

- Pythonサイドカー自体の配布方式が未決定である (ADR-0003参照)
- Tauriビルドには Node.js + Rust toolchain + Visual Studio Build Tools が必要で
  開発環境セットアップが複雑になる
- WebView2はWindows 11に標準搭載されているが、Windows 10では別途インストールが必要な場合がある

### 不採用: PySide6 + PyInstaller 構成

| 評価軸 | PySide6 + PyInstaller | Tauri + Rust |
| --- | --- | --- |
| プロセス分離 | 追加設計が複雑 | 自然な構造として実現できる |
| 低遅延キャプチャ | GIL経由のオーバーヘッドあり | WASAPIをネイティブに扱える |
| Win32 API統合 | pywin32ラッパー経由 | 型安全なバインディング |
| インストーラーサイズ | Python環境全体を同梱 | Rustバイナリのみコンパクト |
| UI実装 | Qtウィジェット | Web技術 (React) |

詳細は実装計画書の「付録A: 検討済み・不採用構成」を参照のこと。
