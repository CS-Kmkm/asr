# 日本語モデルの導入・比較（Issue #88）

測定：2026-11-10 JST、実装・検証の更新：2026-11-11 JST。

2026-11-11の指示により、今回の範囲はWindowsネイティブでの導入実装です。
Haas／Parakeetの管理接続は実装済みで、追加の精度・速度・量子化評価は後回しにします。
Linux／WSL／Docker／公式NeMoの導入は今回の対象外です。以下の測定結果は既存の記録として保持します。

Haas-ja-v1.0とParakeet Japaneseを、Windowsの管理対象speech.cpp/Vulkan経路として追加しました。
既定のfaster-whisperは維持しています。今回の公開固定セットではHaasが有望でしたが、
実用録音とstop→confirmed pasteの比較が完了するまでは標準モデルの選定ゲートを通過扱いにしません。

## 利用方法

モデル画面で「日本語モデル（Vulkan）」を選び、HaasまたはParakeet Japaneseを選択して
「モデルを読み込む」を押します。初回は約690 MBのランタイムと重みを取得し、SHA256を検証します。
Windows x64とVulkan対応GPUが必要です。speech.cppのサーバーを手動起動する必要はありません。
非対応プラットフォームでは選択／読み込みを無効化し、workerもOSとCPUアーキテクチャを取得前に確認します。
対応状況が未確認ならGPU診断への案内を表示します。日本語以外の認識言語との組合せは設定保存とロード画面で拒否します。
Whisperへ戻した場合の量子化設定も保持します。
GPU経路はVulkanであり、CUDAではありません。現在はQ8_0固定です。

モデルは日本語専用です。認識言語の明示指定が日本語以外ならエラーになり、他のモデルへ送信しません。
自動言語設定はモデルの言語検出を意味せず、日本語として認識します。Rustは自動設定を `null` として渡します。
英語にはWhisperを選んでください。認識時の失敗は、既存の日本語対応の共通エラー／履歴の再試行案内で通知します。
取得・起動・GPUメモリ不足等の失敗時は、モデル画面でfaster-whisperを選択して再読み込みできます。
クラウドへの自動フォールバックはありません。

保存先は `%LOCALAPPDATA%/LocalVoice/speech.cpp` です。
開発時は `ASR_SPEECH_CPP_HOME` で別の保存先を指定できます。
既存ファイルのハッシュが違う場合はロードを拒否し、そのファイルを上書き・削除しません。
取得・展開は一時ファイルを検証してから原子的に配置します。1時間以上経過し、作成元のworkerが
終了したことを確認できる専用の一時ファイルだけ、次のロード時に回収します。
子サーバーの環境変数はWindowsの実行に必要なものに制限し、APIキー・HF_TOKEN・proxy設定は渡しません。
自動キャッシュ削除・任意GGUFの導入・全モデル共通のメタデータ画面は#87の範囲です。

## 固定した実行物

2026-11-10 JSTに確認した最新安定版は
[speech.cpp v0.10.1](https://github.com/nyosegawa/speech.cpp/releases/tag/v0.10.1)（2026-11-01公開）です。
Windows配布物 `speech-0.10.1-windows-x64-vulkan.zip` のSHA256は
`a8fe0e4b4f463c460e7ace9d38c363bec08b254cac5a7f764b7967af35d4fa1a`。
アーカイブと、展開した実行ファイル・DLLの双方を固定ハッシュで検証します。

| モデル | 変換元／変換重み | 変換重みrevision | 形式・ライセンス |
|---|---|---|---|
| Haas-ja-v1.0 | [Third Intelligence](https://huggingface.co/third-intelligence/Haas-ja-v1.0)／[sakasegawa](https://huggingface.co/sakasegawa/Haas-ja-v1.0-GGUF) | `6b965d5007ba780585de51549d57a8c1c0d67c7e` | GGUF Q8_0、Apache-2.0 |
| Parakeet Japanese | [NVIDIA](https://huggingface.co/nvidia/parakeet-tdt_ctc-0.6b-ja)／[sakasegawa](https://huggingface.co/sakasegawa/parakeet-tdt_ctc-0.6b-ja-GGUF) | `f7cb5387e1e76385456b7a792054565ecf177417` | GGUF Q8_0、CC-BY-4.0 |
| Whisper turbo | [mobiuslabsgmbh/faster-whisper-large-v3-turbo](https://huggingface.co/mobiuslabsgmbh/faster-whisper-large-v3-turbo) | `0a363e9161cbc7ed1431c9597a8ceaf0c4f78fcf` | CTranslate2、CUDA float16（引数はbf16） |

Haasのファイルは `Haas-616M-ja-v1.0-Q8_0.gguf`（656,381,472 bytes）、SHA256は
`cd5f60b2f1c411404b83fca4a65e3ce30a26d9e951b6a346529e65f22758afeb`。
Parakeetは `parakeet-tdt_ctc-0.6B-ja-Q8_0.gguf`（661,012,416 bytes）、SHA256は
`6ff90c5240f0778dfb44816916eb3c8e634645e120170d6ae43c0f50a11d79d0`。
GGUFのrevisionはNeMoのcheckpoint revisionとは別物です。

## 固定公開音声の結果

同一PC：Windows 11、RTX 3060 12 GB、ドライバー610.88、Core i5-13400。
#86の `practical-ja-holdout-v1` を使用しました。FLEURS日本語100件とJVNV regular100件です。
同じ16 kHz mono PCM WAV、同じja-JP、辞書・補正なしで実行しました。
公開音声の作成方法・ライセンス・固定lockは[評価セットの手順](eval_set/public_datasets.md)を参照してください。

各モデルにつき、プロセス再起動を伴う1周200要求と、別プロセスでのwarm各2回400要求です。
CERは各音声の先頭warm出力だけを採点し、200件を分母にしています。反復を独立音声として数えていません。
Rawは元の参照文との文字編集率、正規化は既存scorerのNFKC・空白／句読点等の規則です。
全体CERは編集数／参照文字数のmicro集計で、Haas公式のjacer指標とは異なります。
日本語WERは分かち書き規則を設けていないため報告しません。

| モデル | Raw CER | 正規化CER | warm要求 p50 / p95 | プロセス起動後load要求（1回） |
|---|---:|---:|---:|---:|
| Haas Q8_0・Vulkan | 8.16% | 4.32% | 55.92 / 96.87 ms | 1,502 ms |
| Parakeet Japanese Q8_0・Vulkan TDT | 13.03% | 8.61% | 52.75 / 94.17 ms | 1,984 ms |
| faster-whisper turbo・CUDA float16 | 10.48% | 4.78% | 389.92 / 507.92 ms | 32,411 ms |

warm要求の時間はworkerのJSONL要求→応答のwall時間です。speech.cppはその内側にlocalhost HTTPを含みます。
ロード済みサーバーは既定の起動時warmupも実行します。バッチスループットやstop→pasteの値ではありません。
load要求はモデル検証・読込・起動／warmup等を含み、Pythonプロセス生成時間と初回ダウンロードは含みません。
キャッシュ・GPU shader・OSファイルキャッシュは消していないため、disk-coldの比較でもありません。
既存harnessのcold配列は1回のロード後に200件を連続処理するため、配列全体を「200回のcold試行」とは扱えません。
測定は通常のデスクトップ上の探索的比較で、同セッションで一部回帰テストも実行しました。専用ベンチマーク環境の保証はありません。

ローカル証拠は `bench_data/results/issue-88-{haas,parakeet,whisper}-holdout-v1` の
`.json`、`.score.json`、`.score.md`、`.gpu.csv` に保存しています。音声と認識文をGitに含めません。
GPU CSVは500ms間隔のデバイス全体の使用量で、他アプリの使用量を含みます。プロセス専用VRAMの値とは区別します。
集計は[本文を含まない結果JSON](eval_set/results/issue-88-windows-vulkan-v1.json)に保存しました。
観測されたデバイス全体の最大メモリ使用量はHaas 2,034 MiB、Parakeet 1,984 MiB、Whisper 3,651 MiBです。
ロード後の最初の認識要求はそれぞれ5,172 / 154 / 415 msでした。
Haasの初回形状のGPUコンパイル等を含みうる値で、warm中央値でcold体験を代表できません。
ロード後の1試行だけなのでcold p95は算出していません。
最初の比較記録のdevice_policy欄は共通環境変数 `ASR_DEVICE=cuda` ですが、speech-cppはそれを参照せず
`--device Vulkan0` で起動し、当時のhealthはアダプターが設定した `vulkan/Q8_0` ラベルでした。
この時点のGPU利用はデバイス一覧とnvidia-smiで確認したもので、health自体が物理GPU名を返していたわけではありません。
監査後は、サーバーのmodels応答のdevice／weight_typeを検証し、同じ実行物のdevices一覧に照合して
`deviceName` を記録します。標準キャッシュからの再認識で両モデルの `NVIDIA GeForce RTX 3060` を確認しました。
harnessを修正し、以後のspeech-cpp記録ではpolicyを `Vulkan0` と明記します。

数字はNFKC後の数字列53出現（35音声）の表記一致を確認し、Haas 41/53、Parakeet 45/53、Whisper 45/53でした。
否定表現の限定的な文字列検査（`なかった|ません|ない`）は52出現／42音声で全モデル52/52でした。
これは参照と出力の出現数を照合した未整列の表記保持検査です。数字の読み・単位・否定の意味や位置を正しく認識した証明ではありません。
既存の専門語リストの参照文出現は0件で、固有名詞／専門語の比較を満たすセットにはなっていません。

## 長尺・無音・プレビューの追加試験

固定dev集合の先頭FLEURS音声を250msの無音付きで反復し、先頭3/10/30/60秒を切り出した
同じ16 kHz mono PCM WAVを各モデルに5回ずつ渡しました。自然な継続発話でも精度評価でもなく、
長さに対する負荷と文落ちの探索です。先頭試行も含むため厳密なsteady-stateとは区別します。
元音声IDとSHA256、CPU・RSS・プレビュー集計は[追加試験JSON](eval_set/results/issue-88-stress-v2.json)に保存しています。
v1集計の説明にあったwarm5という表記をv2で訂正しました。元の測定値とv1ファイルは保持しています。

| モデル | 3秒 p50 | 10秒 p50 | 30秒 p50 | 60秒 p50 / 最大 |
|---|---:|---:|---:|---:|
| Haas | 19.83 ms | 56.32 ms | 122.11 ms | 287.79 / 3,019.81 ms |
| Parakeet Japanese | 22.64 ms | 56.42 ms | 108.01 ms | 192.45 / 217.82 ms |
| Whisper turbo | 299.89 ms | 498.53 ms | 3,482.70 ms | 6,712.41 / 6,762.19 ms |

この表はPython backend呼出時間で、前表のJSONL往復時間とは境界が違います。
Parakeetの出力は10秒74文字に対して30秒74文字／60秒73文字で、繰り返し区間の脱落が見られました。
Haasは65／162／337文字、Whisperは62／151／322文字でした。文字数だけで全文の正しさは判断していません。
VADなしの一括認識を長尺で採用する根拠にはできません。

10秒のデジタル無音はHaasとParakeetが5回とも空出力、Whisperは5回とも14文字の出力でした。
Whisper standardはVADオフの条件です。アプリの実マイク・背景雑音・無音判定を含む試験ではありません。

60秒試験5回の平均CPU時間（Python＋speech.cpp子プロセス、複数コア合計）は
Haas 2,250 ms、Parakeet 1,091 ms、Whisper 8,169 msでした。
同区間の最大RSS合計は209.3 / 179.1 / 1,069.1 MiBです。モデルを同じPythonプロセスで順番にロードしたため、
以前のライブラリやアロケーターの影響を含み、モデル単独の必要RAMとは主張しません。

既存harnessで3秒の模擬録音＋直列プレビューを各3回実行したstop→final-responseのp50/p95は
Haas 76.51/130.11 ms、Parakeet 84.16/116.65 ms、Whisper 496.30/511.03 msでした。
in-flight previewの待ち時間と最終認識を含む値で、マイク停止・VAD確定・IME・clipboard・貼り付け確認は含みません。
全長の同じ音声を繰り返すプレビュー負荷試験であり、リアルタイム音声ストリームの品質評価でもありません。

## CUDA／CTCと配布負担

[Haas公式の要件](https://github.com/third-intelligence/Haas-ASR#requirements)は
Linux・NVIDIA GPU・Python 3.11以上・NeMo 2.7です。KDAにflash-linear-attentionとTriton/CUDAが必要です。
公式の依存ファイルはNeMo 2.7系とflash-linear-attentionを制約しています。
現在のWindows用Python環境へ直接混ぜる導入は行いません。
2026-11-11の指示により、Linux／WSL／Docker／公式NeMo/CUDA経路は今回の対象外で、実装完了の条件ではありません。
公式NeMo/CUDAの同一GPU実測は行っていません。従来のWSL/Docker不使用制約は背景情報であり、環境の追加を次の必須作業にはしません。
上記のWindows/Vulkan測定をNeMoのCUDA性能と同等とは主張しません。

[speech.cppのFastConformer仕様](https://github.com/nyosegawa/speech.cpp/blob/v0.10.1/docs/models/fastconformer.md)では、
両モデルのデコードはgreedy TDTで、量子化版のデコーダーはCPUで動作します。
Parakeet JapaneseのCTCヘッドは未対応です。multilingual v3は日本語モデルではなく、今回使用していません。
公式NeMo版のTDT/CTCやBF16との比較は今回の対象外です。
量子化比較そのものがLinux必須という意味ではありません。Windowsでの別形式・別実装の比較も未実施で、
現在の製品実装はQ8_0固定です。非公式Triton移植を含む別のWindows CUDA経路は導入しません。

## 辞書と長尺音声

Haas公式は `{表記: [読み...]}` のVocabularyを持ち、GPU phrase boostingと表記置換を行います。
speech.cpp v0.10.1にもHaasの `keywords`／`keywords_alpha`／`keywords_guard` があり、
[HTTP仕様](https://github.com/nyosegawa/speech.cpp/blob/v0.10.1/docs/server.md)はkeywords配列を扱えます。
既存辞書のsurfaceを表記、readingとaliasesを候補読みとして渡す設計が可能ですが、
aliasesが読みなのか表記なのかを区別すること、語数・対象アプリscopeの制限、無音での誤boostを検証する必要があります。

今回は既存の自由文promptを不確実に逆解析して渡さず、認識後の既存辞書置換を維持しました。
ネイティブboostは未接続です。用語・数字の量子化影響も未検証であり、Q8_0の結果だけから推論できません。
TDTの全体認識はstateful streamingではありません。既存プレビューも繰り返しbatch認識です。
長尺の文落ち対策として、公式は無音位置での分割／speech.cppのVAD領域分割を案内しています。
今回の管理対象サーバーには別VADモデルや自動分割は追加していません。

## 選定ゲートと残る確認

Haasの正規化CERはWhisperより約0.46ポイント低い値でしたが、200件・2ドメインの結果だけで
自然な会話や専門語の優位性、導入安定性、統計的優位性は確定しません。
FLEURSとJVNVもHaasの開発元評価に使用されています。train/test重複がないことの証明にはならず、
JVNVは複数話者でスクリプトを共有します。ReazonSpeech／Common Voiceは今回の主比較には使用していません。
詳しい既知／未知の学習重複は[公開データの注意事項](eval_set/public_datasets.md#学習データ重複の扱い)を参照してください。

## 実画面での入力確認（2026-11-11）

通常利用のアプリとは別の識別子を持つデバッグ版で、アプリ起動→文字起こし開始→ユーザーによる
セキュリティ権限確認・テスト録音停止→Computer Useでの検証の順に進めました。
各試行でも実画面の「録音中」を確認してから、同じ公開FLEURS音声9.3秒を仮想ケーブルへ再生しました。
3モデルとも停止後に専用のWindows EDIT欄へ文字が入り、元のUnicodeクリップボード内容への復帰を観測しました。
プレビューはオフ、文章補正と自動音声モードはオフ、日本語指定、各モデル1試行です。

| モデル | 停止→最初の入力観測 | 停止→元のUnicode内容への復帰の観測上限 | 音声前／後の録音待ち時間 |
|---|---:|---:|---:|
| Haas | 939 ms | 964 ms | 22.83 / 24.35秒 |
| Parakeet Japanese | 779 ms | 804 ms | 14.52 / 11.29秒 |
| モデルID未指定のfaster-whisper | 1,424 ms | 1,441 ms | 19.67 / 11.93秒 |

[文字列を含まない試行記録](eval_set/results/issue-88-native-smoke-v3.json)に音声・実行ファイル・補助ソースのSHA256、
日付と設定、元記録のSHA256を残しています。試行番号1・3・5はarmコマンドの番号で、2・4・6は各試行の再生コマンドです。
間の番号は除外された録音ではありません。3回準備し、3回再生し、3回の入力・Unicode復帰を観測しました。
旧集計と元記録は上書きせず、v2・v3は同じ3試行の定義・来歴・表現だけを補足しています。
WhisperはモデルID未指定・デバイス自動・読み込み設定bf16で、
この試行の実モデルrevision・デバイス・compute typeは独立に採取していません。上の固定batch比較との同一性は主張しません。
メインエージェントの画面観察ではWhisperの末尾に元音声にない定型句がありました。画面記録・文字列は未保存です。
3モデルとも入力は目視しましたが、同じ基準で全文を採点していません。Whisperの約11.93秒の末尾無音を含む1例であり、一般化しません。

「停止」はComputer Useが送った停止キーを補助の低レベルフックが観測した時点です。時計は `time.perf_counter()` です。
最初の入力はEDITのEN_CHANGE通知または読み取りポーリングで観測した非空の変化で、実際の入力時刻そのものではありません。
復帰上限は、最後のクリップボード番号変化を観測した時刻を、その後250ms以上安定してから元の文字列との一致を読み取って検証した値です。
検証の読み取り時刻を停止からの時間として計上していません。主ループの待ちは公称5msですが、実際の最大観測間隔は未記録です。

これは動作確認です。録音前後の待ち時間がモデルごとに異なり、p50/p95、入力失敗率、速度順位の根拠にはなりません。
EDIT欄への入力は型打ちによるフォールバックと区別できず、アプリ内部のconfirmed-paste時刻でもありません。
復帰確認はUnicode文字列の一致だけで、クリップボードの全形式・履歴除外フラグ・履歴／同期は未検証です。
計測補助は文字列を出力せず、クリップボードを書き換えません。入力観測と250msの状態安定を待って短時間読み取るため、
無干渉とは言い切れません。ほかのアプリがコピーや入力をした試行は比較対象にできません。

最初の中断試行では旧計測補助の終了処理が失敗し、その時点のクリップボード復元は確認できません。
今回の成功は当時の内容を復旧したことを意味しません。[事象記録](incidents/2026-11-11-e2e-permission-and-clipboard.md)のとおり、
旧試行は集計から除外して保持しました。補助の修正は独立監査と限定再確認を経て、合成テストでも確認しています。
v1〜v5の独立確認後、元内容が `None` の場合を復帰成功にしない等の追加修正を合成テストで確認してから3試行を実施しました。
空のUnicode文字列 `""` はこの除外に含まれません。元の内容は保存・ハッシュ化していないため、空文字列だった可能性は否定できません。
この最後の追加修正だけの独立再確認は録音前には行っていません。記録の補助SHA256は11件のテストと実測に使ったファイルに一致します。
3試行後のPython全回帰で353 passed / 1 skipped / 217 subtestsが通過しました。
実測後の独立した証拠・技術監査と限定再確認で、数値と定義を照合し、最後にこの `None` と空文字列の表現差を修正しました。

通常利用のアプリは起動したままで、計測用とはショートカットが異なります。他のクリップボード監視が存在しないことまでは確認していません。
計測前のクリップボードはその時点の内容を読み取っただけで、新しい目印を用意していません。旧補助の目印が残っていた可能性も含め、内容の由来は未確認です。

## 再測定と検証

固定セットを準備したリポジトリのルートから実行します。出力は毎回新しい名前にしてください。
以下のモデルIDとrevisionを上の表に従って変えるとParakeetも測定できます。

```powershell
$env:ASR_MODEL_ID = "haas-ja-v1.0"
uv run scripts/bench_asr.py --backend speech-cpp --quantization 8bit `
  --manifest bench_data/manifests/practical-ja-holdout-v1.json `
  --repeat 2 --cold-runs 1 --record-device `
  --model-revision 6b965d5007ba780585de51549d57a8c1c0d67c7e `
  --runtime speech.cpp-0.10.1 --json --output bench_data/results/my-new-haas-run.json
uv run scripts/score_asr.py --bench bench_data/results/my-new-haas-run.json `
  --manifest bench_data/manifests/practical-ja-holdout-v1.json `
  --output bench_data/results/my-new-haas-score.md
```

Whisperは同じmanifest・repeat・cold-runs・record-deviceを使用し、backendを `faster-whisper`、
量子化を `bf16`、`ASR_DEVICE=cuda`、model ID・revisionを表のWhisperに変更します。
runtimeは `ctranslate2`、`--whisper-profile standard` を明示します。
任意のruntime/model revisionラベルはharnessが自動検証する値ではなく、キャッシュの実体を確認して指定する値です。

Python全回帰342 passed / 1 skipped、Rustライブラリ875 passed / 7 ignored、
フロント全回帰189 passed（新規モデル選択テストを含む）、ビルドとClippyが通過しました。
初回ランタイム実ダウンロード・SHA256検証、モデルの実ロード／認識／終了、
子サーバークラッシュ→エラー→再ロード、worker強制終了→子サーバー5秒以内終了を実機で確認しました。
GPU不在や実OOMは発生させておらず、起動失敗の安全な処理は合成テストによる検証です。

200件の比較・負荷試験は子プロセス環境の制限と監査後の取得／終了処理修正より前の値です。
修正後は[再認識の集計](eval_set/results/issue-88-post-audit-smoke-v1.json)で、同じholdoutの3音声ずつを
全モデルで再認識し、各3/3件が元の先頭warm出力と完全一致しました。これは200件の再測定ではありません。
Windows Job Objectによるクラッシュ復旧・worker強制終了時の子サーバー終了も修正後に再確認しています。
独立監査で指摘された固定モデルへの任意ID追加、展開の原子性、一時ファイル回収、終了時の状態初期化を修正し、
ハッシュ不一致・不完全zip・既存破損ファイル保持・終了タイムアウト・環境変数の許可／拒否を合成テストで確認しました。
さらにWhisperの量子化設定を永続化後も保持すること、非対応OS／CPUで取得を始めないこと、
不正IDを受理しないこと、解除失敗時もworkerのloaded状態を消すことを回帰テストで確認しました。
実配布zipからの原子的展開は、ランタイム未展開の新規ディレクトリでも[実機検証](eval_set/results/issue-88-runtime-extract-v1.json)しました。
この試験は取得済みのzipと重みを事前配置したもので、ネットワーク再取得やGUIの操作試験ではありません。
アプリからworkerへの `LOCALAPPDATA`／`ASR_SPEECH_CPP_HOME` 継承と、任意のworkerソースを含む配布設定も確認しています。
`claude-opus-5-5` に `--effort high` を指定した独立ソース／証拠監査と限定再確認を完了しました。
回答モデルとツール無効化をメタデータで確認しました。管理接続のソース監査v1〜v4の指摘は修正・再検証済みで、その限定再確認では未解決の確認済み欠陥はありませんでした。
後続の計測補助・証拠の監査v1〜v7と、その後の保守的な変更は実画面確認の節に別途記載しています。
今回のWindows限定・評価延期への範囲変更は、これらとは別の文書監査と限定再確認を完了しました。
記録は `.ai/test-results/issue-88-windows-scope-response-v1.jsonl` と `issue-88-windows-scope-response-v2.jsonl` です。
この範囲変更時点でも機能回帰を再実行し、Python353 passed / 1 skipped / 217 subtests、
Rust875 passed / 7 ignored、UI189 passed、ビルド・Clippyを確認しました。
対象ソース240ファイルのSHA256一覧はチェック前後で一致しました。一覧はマニフェストに、集約値と保存先はタスク契約に記録しています。
追加の音声性能評価は行っていません。
これはコードと記録の監査で、レビュアーがGUIや実用録音を操作したことを意味しません。

今回のWindows導入実装と、モデルの採用判断は分けて扱います。
自然会話・小声・言い直し・専門語等の評価、同条件のstop→confirmed paste p50/p95・失敗率、
長尺・プレビュー・量子化の追加比較は後回しです。公式NeMo経路は今回の対象外で、実装完了の条件ではありません。
既定変更は将来の評価後に別途判断します。追加評価を延期したことは、選定ゲートを通過したことを意味しません。

実装と検証の追跡は[タスク契約](tasks/issue-88-japanese-models.md)を参照してください。
