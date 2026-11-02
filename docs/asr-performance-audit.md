# ASRアプリの実装と高速化候補

調査日: 2026-10-31。対象: `main` の `8d830be` と既存の未コミット8ファイル。
Claude Code セッション `6043cb09-3484-41ae-a331-5050f85cedf3` の網羅的な実装調査を引き継ぎ、現行ソースを確認した。

## 結論

停止後は、音声の確定 → 実行中のライブ認識の終了待ち → 全録音の再認識 → 任意のモード判定 → AI補正または翻訳 → 入力先の安全確認 → 貼り付け、と直列に進む。この順序には改善の余地がある。ただし、現行ビルドの工程別実測はなく、どの改善が何秒効くかは確定していない。

最初に計測の比較条件と時間境界をそろえる。その後、小さく試せる候補は、PCM WAVの専用読み込み、停止要求をライブ処理へ早く通知すること、仮挿入しない場合のAI開始前のキー解放待ちの見直しである。長文向けの本命候補はライブ確定結果の再利用だが、精度・音声境界・キャンセルの設計変更が必要になる。AIのストリーミング受信自体はすでに実装されている。

今回は調査と文書化のみ。高速化のコード実装、設定変更、実機計測、マイク録音、外部プロバイダーへの推論リクエスト、コミットは行っていない。

## 引き継いだ成果と未完了部分

元のworkflowは12領域の探索に成功し、147候補を出していた。個別検証は54件完了（確認5、条件付き確認48、反証1）、93件は利用上限で失敗していた。停止から挿入までを追う専任探索、漏れの点検、最終統合も失敗していた。「147 findings verified」という元ログの文言は、147件の検証成功を意味しない。

| 元の探索領域 | 候補数 | 個別検証の成功数 | 今回の扱い |
|---|---:|---:|---|
| Python worker | 18 | 18 | 現行コード・導入済みライブラリと再照合 |
| Rust ASR client | 10 | 10 | 同上 |
| 音声処理 | 10 | 10 | 同上 |
| pipeline | 13 | 13 | 現行の時間境界・処理順序を再構成 |
| 入力・貼り付け | 16 | 3 | 待機・安全確認・終了処理を再確認 |
| AI補正 | 12 | 0 | transport・SSE・検証・既存差分を確認 |
| live dictation | 8 | 0 | scheduling・音声境界・最終認識との関係を確認 |
| storage | 8 | 0 | retention・WAL・履歴保存の経路を確認 |
| app shell | 14 | 0 | 起動・hotkey・録音開始・常駐資源を確認 |
| frontend | 9 | 0 | 初期化・イベント・デバイス列挙を確認 |
| Translate / Edit / Ask | 14 | 0 | AI呼出し・表示・入力・保存を確認 |
| streaming等の構成案 | 15 | 0 | 上記の現行経路に統合して評価 |

全147候補の原文と履歴上の判定は `.ai/performance-audit-20261031/` に回収した。`candidate-inventory.json` が索引、`recovered-finders.json` が探索全文、`recovered-verifications.json` が54件の検証全文である。今回の報告は重複をまとめた領域別の再評価であり、全147件を個別に再検証したという意味ではない。古いエージェントの効果見積もりを今回の測定結果として採用していない。

## 過去の数値をどう読むか

元workflowのcontextに保存された2026-10-07〜11の集計は、Dictate 16件でp50 4,892 ms、p90 8,674 ms、OpenAI補正16件でp50 2,257 ms、p90 3,012 msだった。今回DBから再集計した値ではない。以前の比較報告にあったp90 9.3秒とは一致しないため、ここでは回収元の値を明記している。

この標本は発話長・成功条件・現行差分の有無を統制していない。[目標](benchmarks.md)の「3秒発話・warm・停止→挿入p50 < 1.5秒、p95 < 2.5秒」と直接比較して達否を決められない。補正p50を全体p50から引いてASR時間を求めることもできない。補正の寄与は調査すべきだが、現状のデータだけで「遅延の半分が補正」「補正の大部分が接続やTTFT」と断定しない。

## 現在の処理経路

以下の行番号は調査時のworking treeによる。Rustの短縮パスは`src-tauri/src/`、`backends.py`/`worker.py`は`asr_worker/`、導入済みライブラリのパスは`.venv/Lib/site-packages/`からの相対パスを表す。主に通常のDictate、最終一括挿入を対象とする。自動モード判定・補正・ライブプレビューの有無で分岐する。

| 段階 | 現在の処理と依存関係 | 根拠 |
|---|---|---|
| hotkey受付 | 設定を読み、ライフサイクルを確保。hold方式のみ誤操作防止の200 msを挟む | `lib.rs:1351,1397`; `commands.rs:779` |
| 録音前準備 | 同意・プロバイダー事前確認 → 元の入力先取得 → アプリ文脈 → モデル準備待ち。UIA等はblocking workerに移して待つ | `commands.rs:918,964,990,1016` |
| 録音開始 | 入力監視helper準備 → 任意の開始音完了 → 辞書prompt → マイクopen/start | `commands.rs:1035,1062,1086,1093` |
| 録音中 | 任意のライブ認識とoverlay表示。レベル通知は80 ms間隔。ライブ認識と対象欄へのライブ挿入は別機能 | `commands.rs:1149,1167`; `live_dictation.rs:31` |
| 停止処理開始 | Processingを確保して状態通知後、captureに渡す`started`を採時 | `pipeline/orchestration.rs:50,83` |
| 音声確定 | audio lock取得、stream終了・join、DSP・WAV作成。音声確定自体はblocking workerで実行 | `pipeline/stop.rs:91`; `audio.rs:496,652` |
| ライブ終了待ち | ここで初めてライブ停止を通知し、task終了を待つ | `pipeline/stop.rs:111`; `live_dictation.rs:104` |
| 最終ASR | 設定・辞書等を取得し、モデル準備を再確認。その後、全WAVをworkerへ渡して最終認識 | `pipeline/stop.rs:204,235,242,248` |
| 任意の自動判定 | 自動セッションがある場合のみローカル分類とモード別preflightを待つ | `pipeline/routing.rs:17,35,76` |
| 補正前 | 生テキストをアプリへ公開 → shortcut解放待ち → 任意の仮挿入 → 補正用辞書取得 | `pipeline/dictate_translate.rs:90,92,100,205` |
| AI | 補正または翻訳を1回要求。SSEをoverlayへ表示し、完成と全体検証を待つ | `pipeline/dictate_translate.rs:149,224`; `correction.rs:645`; `correction/fact_check.rs:73` |
| 挿入前待ち | blocking owner内で入力の静穏・元の入力先を確認。条件を満たせば即座に抜ける | `pipeline/dictate_translate.rs:310`; `live_dictation.rs:245` |
| 出力 | commit fence内で入力先・入力状態・clipboardを確認 → Ctrl+V → 確認 → 必要に応じ復元 | `pipeline/dictate_translate.rs:320`; `injection/batch.rs:499,671` |
| 出力直後 | 結果診断、overlayの完了通知、draft破棄・helper終了を経て`latency_ms`を採時 | `pipeline/dictate_translate.rs:332,347,375`; `input_monitor.rs:298` |
| 次回受付まで | 履歴・辞書候補・metrics保存 → アプリ完了通知 → 一時音声cleanup → guard解放 | `pipeline/dictate_translate.rs:384,425,505,534`; `pipeline/orchestration.rs:66,97` |

### メトリクスの意味

| イベント | 測っている範囲・注意点 |
|---|---|
| `dictation` / `voice_translate` | capture入口から出力後の診断・helper終了まで。hotkeyのdispatch前後の全時間でも、貼り付け瞬間までの厳密な時間でもない |
| `insertion` | 上と同じ`latency_ms`を再利用。成功判定は入力結果。貼り付け単体の時間ではない |
| `asr` | モデルensure完了後のtranscribe往復と直後の処理。モデルload、live drain、WAV作成は含まない |
| `text_correction` / `voice_translation` | AI要求から完成・検証・一部の通知/計測保存まで。純粋な生成時間ではない |
| `audio_stop`〔追加済み〕 | audio lock待ちと音声停止・確定 |
| `live_drain`〔追加済み〕 | stop通知からtask joinまで。録音中のライブ認識総量ではない |
| `*_first_output`〔追加済み〕 | AI開始から最初の空でないcallbackまで。AI全体時間に包含され、接続・server待ち・生成を単独では分離できない。非stream応答なら全文取得時になる |
| `insertion_settle`〔追加済み〕 | 最終挿入前のsettle呼出し。`success=true`は入力先準備完了の保証ではない |
| `insertion_paste`〔追加済み〕 | commit closure全体。入力先確認、clipboard操作、確認・復元を含み、SendInputだけではない |
| `take_spans`〔M1で追加〕 | `metrics`とは別tableで1take 1行。録音開始時のランダムな一時take id、終了outcome 1つ（`pasted`/`paste_unverified`/`clipboard_only`/`insertion_failed`/`panel`/`search`/stage付き`failed`・`cancelled`）と、実際に起きたspanだけを記録（無いspanはNULLで0と区別）。`stop_to_delivery`・`stop_to_next_take`はstop要求起点。診断の`takes`がmode・outcome・stage別に件数・計測数・欠落数・p50/p95を返し、take idは出さない。各spanの定義はREADME参照 |

根拠: `pipeline/stop.rs:91,111,242,335`; `pipeline/dictate_translate.rs:223,238,253,315,320,375,425,438,542`。各stageの中央値は足し算できない。診断画面の分位点には失敗操作の有効な時間も入る（`storage/metrics.rs:126`）ため、成功例限定の過去集計とも区別する。Edit等はさらに時間起点が異なる（後述）。

記録される操作にも偏りがある。`dictation`はdeliveryが計測行まで到達した場合だけ書かれ、その`success`は`!translation_failed`であり、補正fallbackや入力失敗でも成功になり得る。`insertion`成功は`ClipboardPaste`に限る（`dictate_translate.rs:383,425,438`）。音声確定・ASR・routingの途中失敗やcancelは全体イベントの記録前に抜け、`audio_stop`/`live_drain`もcapture成功後に初めて保存される（`pipeline/stop.rs:219`）。従って現在の診断分位点は全停止操作の結果を網羅しない。確認済みpaste、未確認/clipboard-only/失敗、早期失敗、cancelを別の母集団として、全終了経路から対応する時間とoutcomeを記録する必要がある。

## 改善案と優先順位

優先度は、測定・実装に着手する順番であり、速度改善幅の保証ではない。P0は比較の土台、P1は比較的小さく試せる変更、P2は条件や設計変更の大きい候補、P3は主に特定の環境・起動経路向け。

| ID | 優先度・対象 | 確認できた問題と案 | 検証・制約 |
|---|---|---|---|
| M1 | P0・計測全体 | 操作を対応付け、record-ready、model ensure、shortcut待ち、route、helper終了、保存、次回受付までのspanを追加。早期失敗・cancelも含む全終了経路を記録し、現行の計測を上書きして過去と混ぜない | 本文を含まない固定分類と数値。既存の履歴保持・削除方針に従う。確認済みpaste群と、未確認・失敗・cancel群を分け、出力・確認・次回受付を別々に測る |
| M2 | P0・benchmark | productionはlocaleを送るが`bench_asr.py:334`は送らない。idle間隔・ライブ競合も再現しない。実運用条件の比較を追加する | `asr.rs:1261`; `backends.py:718`; `bench_asr.py:393`。同じ音声・locale・辞書・実device/compute typeで比較する |
| A1 | P1・PCM読み込み | faster-whisperはPCM WAVのpathを汎用decodeへ渡す。導入済み1.2.1のPyAV経路には`gc.collect()`がある。16 kHz/mono/PCM16を検証した専用NumPy経路を試す | `audio/wav.rs:10`; `commands.rs:188`; `backends.py:715`; 導入済み`faster_whisper/audio.py:37,69`。配列一致、不正WAV、メモリ、音声load時間を確認。IPC全面変更は不要 |
| A2 | P1・ライブ停止 | WAV確定後まで停止通知しないため、その間に不要なlive decodeが始まり得る。stop通知とjoinを分け、通知を先行させる | `pipeline/stop.rs:91,114`; `live_dictation.rs:104,550`。全操作cancelとは分ける。単にawaitを外してもworkerを使えるとは限らない |
| I1 | P1・補正開始 | shortcut解放を常に待つが、最終一括挿入では直後の`draft.update`は何もしない。この場合のpre-AI待ちを省く/後へ移す候補 | `dictate_translate.rs:92`; `live_dictation.rs:183`; `input_monitor.rs:129`。通常すでにキーが離れていれば効果なし。最終入力前の修飾キー・IME・cancel確認は維持 |
| I2 | P2・挿入前待ち | 入力が変化した場合、settle開始から350 msを新たに数える。最後の入力からの経過を保持し、補正中の静穏時間も利用する案 | `live_dictation.rs:63,245`。入力未変化かつreadyなら既に即時return。実入力、helper再起動、shortcut、IME、focusと最終checkpoint再確認を扱う |
| A3 | P2・preview負荷 | previewと最終認識はprocess共通profile。previewだけ低コスト設定を要求ごとに選べるようにする | `live_dictation.rs:550`; `worker.py:190`; `backends.py:578,715`。既存`fast`はbeamとVADを同時に変えるため、変数ごとに比較。最終認識はまず据え置く |
| A4 | P2・長文ライブ処理 | live loopは各反復で150 ms待った後、現在発話全体をコピー・DSPし、認識可否判断より先にコストを払う。軽い増分VADと完全な音声準備を分ける | `audio.rs:580`; `live_dictation.rs:26,499,515`; `audio/dsp.rs:25,56`。実際の周期には準備・認識時間も加わる。whole-window統計依存のDSPを単純cacheすると結果が変わる。長い連続発話でCPU・allocation・callback欠落を確認 |
| A5 | P2・増分最終ASR | 録音中の確定境界・テキストを返し、停止時は未確定末尾だけ認識する選択式モード | `live_dictation.rs:492,561`; `pipeline/stop.rs:248`。境界・設定・音声処理の同一性と全録音fallbackが必要。重複文字列の削除で接続しない。短文のencoder計算量が音声長に比例して減るとは限らない |
| C1 | P1・既存差分の評価 | 録音開始時のLLM HEAD prewarmは別セッションですでに実装済み。追加実装ではなく、接続再利用の成否とfirst-outputのA/B評価が残る | `correction/transport.rs:79`; `commands.rs:928`。first-outputだけでは接続再利用を判定できず、本文・認証headerを含めない接続ID/新規TCP・TLS確立数のtransport計測が別途必要。同じpoolでも再利用の保証はなく、cancelした録音でも通信する |
| C2 | P2・補正方針 | 短文補正省略、補正期限で生テキストfallback、モデル/プロバイダー比較は、呼出し回数や待ちを減らせる候補 | `dictate_translate.rs:204`; `transport.rs:170,185`; `correction.rs:166`。短文でも辞書・自己訂正の価値はある。品質・コスト・生テキスト出力が変わるため、無断で既定変更しない。2026-11-02 にユーザー判断で見送り（補正方針として採用しない） |
| C3 | P2・投機的補正 | 最終ASRと並行して最後のlive仮説を補正し、最終入力条件が完全一致した場合だけ採用する | `dictate_translate.rs:224`; `live_dictation.rs:561`。一致率を先に測る。本文だけでなく設定・辞書・styleも一致させ、最終検証を通す。miss時の追加料金・未確定内容の送信・cancelの扱いが必要 |
| S1 | P2・保存/次回受付 | 履歴保存ごとのretention処理で、削除なしでもWAL truncateを要求する。削除・再試行状態に応じて実行を絞る候補 | `storage.rs:44,1115,1266`; `storage/history.rs:112`。privacy削除や未完了checkpoint再試行は保持。通常の貼り付け後なので、改善対象は完了表示・次回受付・DB競合 |
| S2 | P2・helper/保存終了 | helper終了は既存latency内、履歴・metrics・一時音声cleanupはその後も受付を塞ぐ。所有権を明示した非同期終了/保存を検討 | `dictate_translate.rs:347,375,384,534`; `input_monitor.rs:298`。タイマー移動だけでは高速化にならない。stale callback、終了drain、Delete all/Never変更、artifact所有権を設計する |
| U1 | P3・起動 | GPU診断を待ってからASR preloadする。診断を期限付きにし、同意・setup条件を保った上で並列化する | `lib.rs:1439`; `commands.rs:3631`。warm停止時間とは別。診断不在・停止、CPU/cloudで確認 |
| U2 | P3・worker回復 | bundled worker spawnごとに`uv sync`、reset後は次の要求で復帰。環境fingerprintと修復fallback、明示的な背景回復を検討 | `asr.rs:682,917`; `worker_setup.rs:32`。warm通常操作に影響しない。依存変化、extras、失敗、shutdown競合、意図しないdownloadを検証 |
| N1 | P3・cloud ASR限定 | desktopはbatch WAV upload・完全JSON待ち。httpxの導入済み既定idle expiryは5秒。接続保持/事前接続やcapability付きstreamingを検討 | `backends.py:819,852`; 導入済み`httpx/_config.py:178,247`。LLM prewarmとは別。毎回再接続とは断定しない。通信・モデル互換・cancel後drain・同意を検証 |

### キャンセルとstreamingの設計上の制約

live decodeをcancelしても、Rust側の背景drainがworker mutexを保持し（`asr.rs:1087`）、faster-whisperのcancel確認はsegment境界である（`backends.py:694,722`）。従って「停止時にlive taskを待たない」だけで最終ASRが即開始するとは限らない。途中のnative decodeを中断するためにworkerをkillすると、モデル再loadのコストと回復リスクが生じる。

最終ASRの増分化は、境界を確定した区間の再利用、最後の区間の認識、全録音fallbackという契約を先に設計する。Whisperの短いwindowはpaddingされる（導入済み`faster_whisper/transcribe.py:1173`）ため、短い発話を細分化すると総計算が増える可能性がある。[以前の単一の合成音声比較](notes/live-dictation-segmentation.md)でも、表示の早さと総計算量は同じ方向に動かなかった。

LLM補正は`stream=true`で受信済み（`correction.rs:556,588,618,645`）。未実装なのは「streamingそのもの」ではなく、全体検証前のテキストを対象アプリへ安全に反映する方式である。保護内容・全文の妥当性・stream完了を検証する前に文単位の追加貼り付けをすると、失敗・cancel・ユーザー編集時に回収できない。既存の仮挿入機構を使う場合も、確認済み範囲の置換と明示的な選択を維持する。最初の表示を早めることと、確定結果を早めることは別指標とする。

## 録音開始・他モード・UI

| 対象 | 結果と候補 | 根拠・制限 |
|---|---|---|
| hotkey→録音開始 | preflight、入力先、モデル、helper、cue、辞書、mic openが直列。各spanと最初の音声callbackを測り、独立した準備だけ重ねる | `commands.rs:918`以降。元入力先のcaptureを遅らせると対象が変わり得る。cue前のmic openやidle常時openは音声・privacy・Bluetooth挙動を変える |
| 自動モード判定 | 有効時のみ最終ASR後にlocal workerで分類。分類中のDictate補正投機は案だが、送信・取り消し・誤分類を要設計 | `pipeline/routing.rs:17,35`; `decision.rs:128`。通常Dictateに常に分類コストがあるわけではない |
| Ask | plan→検証→必要な生成が直列。生成SSEのcallbackを捨て、完成後にpanel公開。panelへの未確定previewは体感改善候補 | `pipeline/ask.rs:158,213,290,387`; `correction.rs:377`; `answer_panel.rs:5`。operation ID、partial/cancel/finalを設計。planner出力を直接実行しない。2要求の安易な統合はaction policyとsource分離を崩す |
| Voice Translate | 補正後に翻訳する二重LLMではなく、直接翻訳を要求 | `dictate_translate.rs:149`; `correction.rs:225`。追加済みfirst-output等を使って測る |
| Edit | stream previewは実装済み。`speak_to_edit`/`insertion`はAI edit開始を起点とし、Dictateのstop起点と異なる | `pipeline/edit.rs:75,211`; `commands.rs:240`。同名insertionを混ぜて比較しない |
| 選択テキスト翻訳 | UIA取得、monitor開始、置換の同期処理がasync owner内に残る。Edit/Askと同じblocking ownerへ移す候補 | `commands.rs:1768,1820,1849`; `pipeline/edit.rs:133`; `pipeline/ask.rs:330`。scheduler応答性の改善候補であり、同じUIA仕事自体が速くなるとは限らない |
| frontend | event駆動で、反復status pollingは見つからなかった。全windowがmain pagesをimport、初期snapshotの`Promise.all`、device列挙の重複は低優先の候補 | `src/App.tsx:1,207,322`; `src/hooks/useAudioDevices.ts:15`; `commands.rs:338`。windowは再利用される。warm ASR遅延の主因という証拠はない |
| diagnostics | SQLite rankingを共有connectionで行うが、ユーザー操作時にblocking taskで実行する | `storage/metrics.rs:121`; `commands.rs:2370`。大きな履歴や他DB操作との競合を測る。毎take自動scanとは扱わない |

## 採用しない断定と後順位の候補

- **「日本語でも毎回言語自動判定」**: 記録された設定`ja-JP`はbackendへ`ja`として渡る（`backends.py:718`）。ベンチマーク側との差を直す。
- **「低域フィルタがない」**: 現行は8次Butterworthを適用する（`audio/dsp.rs:184,199`）。以前の比較報告のこの指摘は撤回対象。
- **「bf16と4bit/8bitをその名称どおり比較できる」**: faster-whisperではCUDA `bf16`設定は`float16`、`4bit`/`8bit`はどちらも`int8_float16`へ対応する（`backends.py:115`）。実compute typeを記録する。
- **「モデル常駐・warmup・HTTP client再利用が未実装」**: すでに存在する（`asr.rs:925`; `backends.py:640`; `correction/transport.rs:61`）。warmupと実decode設定の差、idle後の挙動は実験候補。
- **「全挿入に350 msまたは2秒の固定待ち」**: settleには即時returnがあり、貼り付け確認も初回即時。2秒は確認の期限である（`live_dictation.rs:251`; `injection/batch.rs:511,545`）。clipboard open retryも競合時のみ（`injection/clipboard.rs:190`）。
- **「fact checkでもう1回LLMを呼ぶ」**: 現行はローカル検証（`correction/fact_check.rs:73`）。削除してもremote往復は減らない。
- **「ASR streamingはどの経路にもない」**: 任意のlocal HTTP serverにはsegment SSEがある（`asr_worker/api.py:255,284`）。desktop JSONLは全体完了待ち（`worker.py:223`）。再利用するなら通知eventとoperation/cancel契約の追加が必要。
- **「温度fallbackは全ASRが6倍になる」**: encoder結果は再利用される（導入済み`faster_whisper/transcribe.py:1189,1402`）。fallback回数を測らず短縮を既定にしない。

元候補のうち、beam/temperature/timestamps/VAD/言語/model/quantization/batching/CPU threads、代表的warmup、無音trim、PCM IPC、電源throttling、HTTP/2、promptの短縮/cache、edit-list形式、複数要求のhedging、local LLM preload、Unicode/JSON等のmicro最適化は、条件付きまたは後順位の実験候補として残す。今回の実測なしに採用・速度保証はしない。特に認識設定とモデル変更はCER・用語・数字・否定・自己訂正を評価し、hedgingと投機は追加課金、local LLMはVRAM競合、無音trimは小さい声や語尾の欠落を確認する。UIA/IME/clipboardの安全確認を削って数値を下げる案は採用しない。

## 次の検証と実装順

1. **現行baselineを作る。** まず既存差分を監査・実行可能なbuildへ反映したことを確認。同じ音声で3秒・10秒・30秒超、ready/cold/idle後、preview on/off、補正on/offを分ける。録音再利用と実マイク試験は別に記録し、既存音声・出力を上書きしない。
2. **計測条件と時間境界をそろえる。** locale・辞書・実device/compute type・idle条件をbenchへ渡す。stop→最初の表示、stop→paste送信、stop→確認、stop→次回受付を区別し、各操作を一時的IDで対応付ける。確認済みpaste群と未確認・失敗・cancel群を別に集計し、早期終了で欠けるspanも数える。AIのfirst outputは全体の内数とし、stageごとのp50を合算しない。
3. **小変更を1つずつA/Bする。** PCM専用load、live stop先行、deferred時のpre-AIキー待ちを個別に評価する。既存prewarmは短い/長い録音でfirst-outputをA/B比較する。接続cold/warmと再利用成否は、本文・認証headerを記録しない接続IDや新規TCP・TLS確立数で確認し、first-outputだけから推定しない。
4. **中規模候補へ進む。** preview専用decode、settleのlast-input時刻、retention/checkpointの実行条件、helper/persistenceの終了順を各々評価する。精度や安全の回帰があれば採用しない。
5. **構成変更は選択式で評価する。** 長文の増分final ASR、投機的補正、Ask panel previewを別課題にする。短文補正省略・期限fallback・モデル変更は、品質と体験の判断を伴う。

各条件は最低5回、可能なら10回以上のwarm反復とcold反復を分離する（[既存の測定手順](benchmarks.md)）。少標本のp95には限界がある。正解音声、用語、数字、繰返し、長いpause、小声、noiseを含め、処理時間だけでなくCER・用語再現・挿入結果・追加要求数・メモリも比較する。GUI入力は読み取り可能/不能の両editor、IME変換中、修飾キーhold、ASR中の手入力、focus変更、cancel、clipboard競合、Bluetooth/有線を含め、ユーザーがforegroundを持つ実機で確認する。

## 別セッションの差分と確認状況

`8813f06a…` 由来の8ファイル（README、commands、correction、transportとtests、dictate_translate、stop、storage）には、工程計測とLLM prewarmが未コミットで存在する。今回は変更していない。元ログにはRust 752件成功・6件除外とclippy成功があるが、これは別セッションの実行結果で、今回の再実行ではない。prewarmの独立監査はそのセッションで上限により中断しており、本調査報告の監査と実装全体の検証は区別する。

今回の完了条件は、途中成果回収、現行コードの経路/領域確認、重複をまとめた改善案、計測計画、報告の独立レビューである。アプリの高速化達成やKPI達成を示すものではない。

独立レビューは別のネイティブエージェントで、根拠・技術・論理の観点から実施した。指摘された3点（早期失敗/cancelを含まない計測の偏り、live loopの150 msを固定周期とする表現、first-outputから接続再利用を判断できないこと）を修正し、修正箇所の再確認も完了した。確認範囲で未解決の指摘はない。優先候補のソース・回収件数・時間境界を確認したレビューであり、全147候補の個別再検証や実機性能検証ではない。

優先していた外部Claude Opusへのレビュー依頼は、報告・ソース抜粋の外部送信に対する明示的な許可がないため、自動承認レビューで拒否された。外部送信を行わず、上記のネイティブレビューで代替した。対応記録は `.ai/performance-audit-20261031/review-resolution.md` にある。

検証結果: 元の探索12領域・147候補・個別検証54件を回収し、現行の主要経路と計測定義を再確認した。改善案・制約・測定計画を統合し、独立レビューの指摘を反映した。参照文書と選択したソースの存在、回収件数、`git diff --check`を確認した。既存8ファイルの差分は保持されている。作業契約は[こちら](tasks/asr-performance-audit.md)。
