# Tauri開発起動時のAI補正APIキー未読

- 日付: 2026-08-13
- 環境: [stated] Windows 11、`src-tauri/target/debug/local-voice-input.exe` をTauri開発モードで実行。
- 症状: [stated] AI補正が原文へフォールバックし、直近5件の補正失敗は開始から0〜11msで記録された。
- 原因: [inferred] 開発起動時の作業ディレクトリが `src-tauri` になる一方、`.env` はワークスペース直下にあり、従来の探索候補（作業ディレクトリ直下と実行ファイル隣接）から外れていたため、HTTP送信前のAPIキー取得で失敗した。
- 非原因: [derived] 保存済みのprovider/model/環境変数名は `openai` / `gpt-5.6-luna` / `OPENAI_API_KEY` で正常だった。[stated] 同じキーとリクエスト形式による短文のResponses API呼び出しはHTTP 200となり、`response.completed` を受信した。[derived] 失敗した入力は10〜19文字で、128トークンの最小出力上限不足ではなかった。
- 修正: [decision] 作業ディレクトリ名が厳密に `src-tauri` の場合だけ、親ディレクトリの `.env` を既存候補の次に探索する。無制限な祖先探索は、意図しない秘密情報の読み込みを避けるため採用しない。
- 再現: [stated] `environment_file_candidates(workspace/src-tauri, workspace/src-tauri/target/debug/local-voice-input.exe)` が `workspace/.env` を含まない回帰テストは修正前に失敗した。
- 検証: [stated] 修正後は回帰テストとRustライブラリ全87テストが成功し、`pnpm build` も成功した。[unresolved] 修正後プロセスでの実マイク入力によるE2E確認は未実施。
