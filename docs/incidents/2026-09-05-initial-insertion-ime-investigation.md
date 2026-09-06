# VS Codeへの初回入力とIMEの調査（未解決）

- 日付: 2026-09-05
- 症状 [stated]: ユーザーによると、VS Codeへの最初の挿入で順番が乱れ、一部に未確定文字が混ざる。
- 対象 [unresolved]: エディター、チャット、統合ターミナルのどれかは未回答。IMEのオン・オフによる発生差も未確認。
- 原因 [unresolved]: ユーザー報告の現象を有効な条件で再現できておらず、まだ特定していない。

## 確認した事実

- [stated] クリップボードの本文を取得せず形式番号だけを調べると、Unicodeテキスト、変換用テキスト形式、登録形式が併存していた。現在の `clipboard_snapshot` は2つ目の形式があれば `NotSafelyRestorable` を返す。この条件では `orchestrate_insert` が貼り付けを試さずUnicode入力を選ぶ。
- [stated] [Microsoftの仕様](https://learn.microsoft.com/en-us/windows/win32/dataxchg/clipboard-formats)では、`EnumClipboardFormats` はWindowsが自動変換できる形式も列挙する。複数形式が列挙されること自体は、画像や書式付きテキストを意味しない。
- [derived] `uia_ime_composition_active` はTextEditPatternの取得失敗を `None` にし、Windowsバックエンドはそれを `false` にする。初回挿入の呼び出し側もIME照会エラーを `false` と扱う。取得できない状態と、変換中ではない状態を区別していない。
- [stated] 隔離プロファイルのVS Code 1.136.1は `native-edit-context` を使用していた。付随する `ime-text-area` は読み取り専用だった。
- [derived] Unicode入力の成功判定は `SendInput` が受け付けたイベント数だけであり、編集結果やIMEの確定を検証していない。

## 除外した／採用しなかった観測

- [stated] 最初の「送信成功だが本文が空」という試行は、診断側が読み取り専用のIME補助欄をフォーカスしていた。製品不具合の再現証拠から除外した。
- [stated] 後続試行で入力先がエディター以外に移ったものも除外した。単に成功APIが返ったことを正常入力の証拠にはしていない。
- [stated] 独立したクリップボード用ウィンドウステーションの作成は、サンドボックス外でもOSに拒否された。ユーザーのクリップボードを上書きして代替検証することはしていない。
- [unresolved] 以前のNumLockによる選択失敗とは別の症状。IME自体の不具合、Unicode入力との相互作用、補正デルタとの競合のいずれかは未確定。

## 次の検証

テスト専用VS Codeを前面に保てるタイミングで、実際の編集面をフォーカスした状態のUnicode一括入力を、IMEオフ／オンで比較する。本文の一致、EditContextの更新範囲・未確定イベント、UIAのIME判定を同時に確認する。API補正を通さない入力から始め、そこで成功した場合のみ分割入力・補正を加える。

- [stated] 現在の前面アプリによる前面切り替え拒否で実入力比較を停止した。ユーザーへテスト用VS Codeを前面にできるタイミングを確認中。
- [stated] この調査では製品ロジックを変更していない。一時的な診断コードの取り込みも除去した。
