# 作業契約の索引

現在の進捗は active 契約を参照してください。過去の契約の「次の作業」や環境の記述は、
現在の指示として扱いません。

| 分類 | 契約 | 扱い |
|---|---|---|
| completed | [公開音声ベンチマーク — #86](issue-86-public-benchmark.md) | FLEURS＋JVNV固定、batch実認識60件、実時間streaming replay・採点を実装。独立レビュー完了、PR #98 merge・Issueクローズ・実装ブランチ削除済み。実engine結合は #82 |
| completed | [小音量入力の通知 — #94](quiet-input-feedback.md) | 実機smoke 30件完走（29/30、正規化CER 8.85%で基準同等）、元設定復元済み。極小音量の通知・入力設定導線も実機確認。PR #97 merge・issueクローズ・ブランチ削除済み |
| completed | [PR #90〜#95 のレビュー・統合](pr-90-95-review-and-integration.md) | 5件とCI追補 #96 をマージ、レビュー指摘3件を修正、対応ブランチ6本を削除。最終CI全5ジョブ成功。#81/#86 の残件は継続 |
| active | [残りの高速化](remaining-latency-improvements.md) | WAV直接読込・保存の非同期化・起動/録音開始の並列化・Askの途中表示など。subagentで実装中 |
| active | [停止→挿入の遅延修正](stop-to-insert-latency-fixes.md) | IME誤判定・挿入待ち・入力監視終了・ライブ字幕などを実装。自動テスト・独立監査完了。Windows実機確認は未実施 |
| completed | [ASR高速化の実装調査](asr-performance-audit.md) | Claude `6043cb09…` の途中成果を回収し現行コードを再確認。[調査報告](../asr-performance-audit.md)の独立レビュー・指摘修正完了。高速化実装・実機計測は別工程 |
| completed | [残存ブランチの main 統合](remaining-branch-integration.md) | 自動判定を初期無効で移植。自動テスト・Windowsビルド・独立レビュー完了。GPU実推論と実マイク挿入は未確認 |
| active | [UI polish pass](ui-polish-pass.md) | Remaining inconsistencies fixed; 160 tests/build/review passed; native visual verification pending |
| active | [UI review fixes](ui-review-fixes.md) | Four findings fixed; 155 tests/build/review passed; native keyboard/layout verification pending |
| active | [Remaining explanation cleanup](ui-explanation-cleanup.md) | Redundant copy removed/help consolidated; tests/build/review passed; native visual verification pending |
| active | [Screen density pass](ui-density-pass.md) | Horizontal groups and compact spacing implemented; 151 tests/build/review passed; native visual verification pending |
| active | [Translation language pair](translation-pair-ui.md) | Pair controls implemented; 150 tests/build/review passed; native visual verification pending |
| active | [Icon controls and diagnostics](ui-icons-and-diagnostics.md) | Implementation, automated checks and independent review complete; native visual verification pending due to Computer Use connection failure |
| completed | [Progressive settings help — #72](ui-progressive-help.md) | Hover/focus help and concise settings copy verified; uncommitted |
| completed | [UI refinement — #72](ui-refinement.md) | Implementation, automated checks and native visual validation complete; uncommitted |
| active | [未解決 issue の対応](issue-58-64-error-and-model-loading.md) | 現在の実装、自動検証、実機確認の残件を管理 |
| archived | 上記以外の契約 | 統合済みの作業、旧設計、当時の調査・検証の記録 |

archived はすべての実機条件が検証済みという意味ではありません。残る実機確認や
機能不足は active 契約および対応する GitHub issue に集約します。旧契約内のテスト結果は
実施当時の根拠として残します。
