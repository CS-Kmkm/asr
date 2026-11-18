Goal: Execute reproducible automated acceptance checks for #80–#85 and record measured outcomes and owner-accepted manual criteria in GitHub.

Scope: Offline engine comparisons on frozen Japanese audio, local correction evaluation, and automated dispatch/policy checks. Product GUI timings and subjective blind scoring that require an operator are accepted by owner decision. Keep prior native results as recorded. Do not use private content, incur API charges, or change production settings.

Reuse: Use #86's manifests, bench_asr.py, bench_streaming.py, bench_whisper_server.py, score_asr.py, the correction evaluation dataset, and existing app tests. Store new run artifacts under a fresh .ai directory and preserve earlier results.

Acceptance criteria:

1. Execute and score the available batch/streaming engines on the same fixed input set, with model/device identity, latency boundaries and resource sampling documented. Report unavailable metrics explicitly.
2. Exercise local correction and hybrid execution together with the existing offline policy tests. Report unavailable configured providers separately from manual criteria.
3. Verify Smart routing, operation counts where derivable, and safety behavior through existing automated checks. Identify GUI-only performance observations.
4. Post evidence and dispositions on #80–#85, closing any issue whose applicable criteria are satisfied. Keep outstanding automatable checks or adoption gates visible.

Open questions: Choose an existing pinned manifest, model configuration and repetitions based on available caches and measured run time. Cloud calls requiring provider-specific credentials, payments or consent are not authorized by this task alone.

Context: docs/competitor_comparison.md, docs/japanese_model_comparison.md, docs/tasks/nemotron-runtime-preparation.md, docs/tasks/native-acceptance-continuation.md, and the current Issue bodies.

Status: Offline comparisons and regression checks are complete. Source manifest and 200 WAV hashes verified. The initial 200-item paced run was stopped because it would require at least 37.6 minutes of source time per device. The run used a shared 20-item subset selected before scoring. The local correction guard accepted a wrong-language output and the scorer flagged one meaning-preserving paraphrase. The owner accepted the remaining #80, #83 and #85 criteria for this closeout. The observed findings and measurement limits are recorded below.

## 計測条件

- Windows 11 10.0.26300、GeForce RTX 3060 12 GiB、ドライバー 610.88。エンジンは順番に起動し、専用ベンチマーク機には隔離していません。
- `practical-ja-dev-v1` の200音声を検証し、FLEURS日本語とJVNV regularを各10件選択しました。音源別に `(duration, id)` を昇順でソートし、1始まりの順位6、16、…、96を選びました。派生マニフェストにある件数や層化のメタデータは元セットのものです。音声長は5.78–20.58秒、合計226.19秒です。
- マニフェストSHA-256（元） `9eda6d587cba5c261e6dd5eeee9d851c87ff68ee136b266b0a2ee4d08bcb19b0`。各音声のSHA-256を照合しました。派生マニフェスト `b972268dde6550425a98d8e30da9e5d612d5533dbdfa68fed60167c718b546f8` には今回のPCのローカルパスが入ります。
- faster-whisper large-v3-turbo standard、CUDA int8_float16、OpenWhispr whisper.cpp 0.0.11 CUDA、ggml large-v3-turbo f16 greedy、Haas-ja／Parakeet Japanese Q8_0 speech.cpp 0.10.1 Vulkanを、辞書・AI補正なしで各20件のwarm結果を採点しました。faster-whisper／Haas／Parakeetには別途1ロード＋20要求のcold系列、OpenWhisprには1件のwarmupがあります。Nemotron 3.5 560 ms INT8は100 msごとに音声を実時間で送ります。
- バッチの最終応答はWAVを指定したJSONLの往復、OpenWhisprはmultipart構築後から応答JSONの解析前までのHTTP往復、ストリーミングは末尾チャンク送信後のflush→finalです。停止→確定貼り付けの製品指標には、録音処理、挿入ガード、実アプリへの貼り付けが加わります。

## ASRエンジンの結果

| 条件 | 非空の最終結果 | Raw / 正規化CER | 最終応答 p50 / p95 (ms) | GPU全体の観測ピーク (MiB) |
| --- | ---: | ---: | ---: | ---: |
| faster-whisper CUDA | 20/20 | 12.40% / 6.21% | 455 / 619 | 4,235 |
| OpenWhispr whisper.cpp CUDA | 20/20 | 12.70% / 6.32% | 318 / 479 | 4,690 |
| Haas Vulkan | 20/20 | 9.77% / 5.37% | 112 / 174 | 3,534 |
| Parakeet Japanese Vulkan | 20/20 | 10.16% / 5.79% | 105 / 179 | 3,556 |
| Nemotron CUDA | 20/20 | 21.00% / 16.21% | 358 / 423 | 3,232 |
| Nemotron CPU | 20/20 | 20.80% / 16.00% | 185 / 235 | 2,957 |

GPU全体の値は `nvidia-smi` の時系列をベンチマーク出力ファイルの作成・更新時刻と照合した観測値です。ストリーミングJSONは完了時に一括保存するため、前の方式の終了から当該方式の終了までを集計しました。他プロセスやデスクトップのGPU使用量を含みます。プロセス別VRAMは取得できませんでした。

20件について同じ正解文を採点し、CERは編集距離合計を参照文の文字数合計で割っています。日本語WERの分かち書き規則がないためWERは未算出です。固定専門語リストに該当する箇所は0件でした。20件の遅延p95は観測値です。

Nemotronのfirst partialは、実時間再生開始から非空のpartialを受け取るまでで、CPUのp50/p95は2.50/4.32秒、CUDAは2.59/5.43秒でした。安定確定を示すイベントはありませんでした。再生待ちを含むRTF p50/p95はCPU 1.016/1.029、CUDA 1.035/1.058です。ワーカーツリーの観測最大RSSはCPU 795 MiB、CUDA 1,305 MiBでした。ワーカーの正常final到達は各20/20件です。アプリ側でバッチへ切り替える経路は異常系テストで確認済みで、実操作での発生率は未計測です。今回の正規化CERはバッチの各方式より高く、Nemotronは実験的な選択肢に据え置きます。

過去の製品比較では同一FLEURS 30件で、OpenWhispr 1.11.0は30/30件・停止から最初の貼り付けのp50/p95が685/785 ms、本アプリ main@2ecf602 の増分無効条件は29/30件・591/770 msでした。録音前処理とholdのショートカットは製品ごとの設定です。今回のWindows製品の再計測はオーナー判断で完了扱いとし、今回の数表はエンジン結果として扱います。[以前の製品比較](../competitor_comparison.md#2026-11-03-初回計測製品e2e-b日本語のみ)

評価時のHEADは `6910518` で、既存の未コミットのデバイス表記差分がある `scripts/bench_asr.py` を使用しました。Whisperのofflineキャッシュは `0a363e9` の単一snapshotを確認しました。warm結果のデバイス・型はワーカーの応答を使っています。ソースのハッシュと当時のgit状態を `evaluation-provenance.json` に保存しました。

## 回帰確認

- Rust 965件合格・9件ignored、Python対象110件合格・68サブテスト合格、Vitest 218件合格。
- Smartでは未選択／選択あり／選択不明の分岐、フォーカス・選択変更のガード、ショートカットの押下イベントを記録するトレースを確認しました。通常の1テイクは同一キーと別キーのどちらもtoggleで押下2回、holdで押下1回と解放です。修飾キーの個数、holdの解放、再試行や手動貼り付けはこの押下回数に含めません。同一キーはDictate/Editのキー選択を省きます。GUIでのrecord-ready、停止→貼り付け、実利用タスクの成功率はオーナー判断で完了扱いとします。
- 補正セットは36ケースと57個の追加検証出力を確認しました。正解文は36/36、原文は21/36が決定的ルールを満たし、追加出力の期待値も一致しました。これらは採点器の対照です。

## ローカル補正とASRの組み合わせ

llama.cpp b11193 Vulkan と Qwen2.5-1.5B-Instruct Q4_K_M を評価ディレクトリに取得し、両ファイルの公開SHA-256を確認しました。Vulkan0、GPU offload、context 4096、1スロットで、loopbackに限定して起動しました。本アプリの `correction_eval` 経由で実際の補正プロンプト、ローカル送信経路、検証ガードを通しました。[ランタイムのリリース](https://github.com/ggml-org/llama.cpp/releases/tag/b11193) · [モデルとSHA-256](https://huggingface.co/Qwen/Qwen2.5-1.5B-Instruct-GGUF/blob/dd26da4/qwen2.5-1.5b-instruct-q4_k_m.gguf)

| 固定36ケースの結果 | 補正前の原文 | 実ローカル補正後 |
| --- | ---: | ---: |
| 決定的ルールに合格 | 21/36 | 27/36 |
| 情報欠落なし | 34/36 | 31/36 |
| 名前・数字を保持 | 35/36 | 35/36 |
| 否定を保持 | 11/11 | 10/11 |
| 命題を保持 | 35/36 | 34/36 |
| 自己訂正を解決 | 0/8 | 3/8 |

ガードは36候補中31件を通し、5件で原文に戻しました。通過した6件は決定的ルールに不合格です。原文が合格し、補正後に採点器が命題欠落を報告した2件を出力と照合しました。`ja-instruction-not-executed-01` は意味を保った言い換えで、採点器の誤判定でした。`ja-instruction-summary-01` は命題を保っていますが、日本語入力の補正で中国語を返し、ガードを通過しました。`ja-negation-unrelated-repair-02` は採点器が否定欠落を報告し、実際の出力では否定表現は残り、指示と自己訂正の関係が崩れました。読みやすさ・語調のブラインド比較はオーナー判断で完了扱いです。

公開音声20件の認識結果を同じローカル補正へ送り、同じ参照文で再採点しました。正規化CERはfaster-whisperで6.21%→9.47%、Nemotron CUDAで16.21%→24.53%で、40件中2件はガードが原文に戻しました。補正後のCERは参照文との文字単位の差を示し、意味や可読性は測りません。

- llama-serverの生成から `/health` 成功・リスナーPID一致まで3,417 ms。固定セットと認識結果セットで、ローカルの生成完了ログは76件でした。サーバー内処理のp50/p95は固定36件で129/388 ms、認識結果40件で182/379 msです。CLIの全体時間はそれぞれ28.05秒と8.59秒です。
- GPU全体の観測値はサーバー起動前2,843 MiB、ロード後3,958 MiB、アイドルのLLMとfaster-whisperを同時にロードした区間でピーク5,456 MiBでした。faster-whisperはCUDAのまま20件を完走し、warm p50/p95は406/592 msでした。GPU全体の値にはデスクトップ等が含まれます。サーバー単体のプロセス別VRAMは未取得です。
- この計測では評価ツールがサーバーを起動し、保存済み認識結果を順に補正しました。製品の自動起動、両モデルの同時推論、実アプリへの貼り付け、承認済み実クラウド構成は未計測です。クラウド同意、オフライン、エラー時のフォールバックは既存の回帰テストで確認しました。

## 再現と結果の保存先

- 同じPCでは `.ai/november-auto-20261118-142209/` の `asr-manifest.json` を全方式に渡し、`bench_asr.py --repeat 1 --cold-runs 1 --record-device --record-resources`、`bench_whisper_server.py --repeat 1 --warmup 1`、`bench_streaming.py --chunk-ms 100 --record-resources --timeout 120` で実行できます。CPUとCUDAのアダプターは `asr_worker.streaming_replay --device cpu` と `--device cuda`、採点は `score_asr.py --manifest … --terms docs/eval_set/terms.txt` を使用しました。詳しいモデル・実行設定は各結果の `config` にあります。
- 主要集計は `asr-summary.json`、`local-evaluation-summary.json`、`local-correction-score.json`。比較用スクリプトと元データは同じディレクトリにあり、モデル・公開音声の認識文を含む生データはローカルに保管します。

## 完了判定

- #81、#82、#84は自動評価の結果とオーナー承認の実操作範囲を記録して完了。
- #83はオーナー判断で完了。ローカル構成を実測し、実クラウドの接続・費用・品質・遅延の比較は未測定として記録しました。
- #85はオーナー判断で完了。人手ブラインド評価の扱い、異言語出力のガード通過、意味を保つ言い換えに対する採点器の誤判定を記録しました。
- #80はオーナー判断で完了。今回の評価時点の標準モデルは維持し、Nemotronは実験的なopt-inです。
