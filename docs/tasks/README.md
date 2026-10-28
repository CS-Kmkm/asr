# 作業契約の索引

現在の進捗は active 契約を参照してください。過去の契約の「次の作業」や環境の記述は、
現在の指示として扱いません。

| 分類 | 契約 | 扱い |
|---|---|---|
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
