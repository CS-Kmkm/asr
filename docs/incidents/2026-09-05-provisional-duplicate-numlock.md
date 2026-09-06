# NumLock有効時に仮入力と補正結果が二重に残る問題

- 日付: 2026-09-05
- 環境 [stated]: Windows、NumLock有効、標準の複数行EDITコントロールで実機再現。
- 症状 [stated]: 仮入力 `draft` を `corrected` に置換すると `correcteddraft` が残った。
- 原因 [stated]: `replacement_inputs` の左矢印に `KEYEVENTF_EXTENDEDKEY` がなかった。実際のキーイベントを観測すると、左矢印の前にShift解放が発生し、選択範囲が `(0, 0)` になった。左右端が `(0, 5)` の選択になるべき箇所だった。
- 裏付け [stated]: [Microsoft PowerToysのSendInput資料](https://microsoft.github.io/PowerToys/modules/keyboardmanager/keyboardmanager/)は、矢印に拡張キーフラグを付けないとテンキー側のキーが送られ、NumLock有効時に問題が生じると説明している。
- 除外した説明 [derived]: 今回の再現はASR、API、IME確定処理を使わず成立した。API完了イベントの全文再送やIMEだけでは、この再現を説明できない。
- 修正 [stated]: 左矢印の押下と解放の両方に `KEYEVENTF_EXTENDEDKEY` を追加。同じ実機で、キー指定だけを変えると選択・置換が成功した。
- 検証 [stated]: 通常のRustテスト92件成功。対話デスクトップが必要な1件は通常実行では除外し、別途明示実行して成功。英語、日本語、LF、CRLFを含む仮入力を一括キー送信で置換し、前後の既存テキストも保持した。
- 残る検証範囲 [unresolved]: ユーザーの入力先アプリは未確認。今回の実機テストは標準EDITコントロールであり、全アプリの選択・IME挙動を保証するものではない。

実機回帰テストはNumLockを有効にし、入力操作を止めた状態で実行する。テスト用入力欄を一時的に前面化し、終了時に閉じる。

```powershell
cargo test --manifest-path src-tauri/Cargo.toml --lib native_replacement_preserves_surrounding_text_with_numlock_on -- --ignored
```
