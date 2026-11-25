# asr の評価スイート

Phase 2 以降の変更を、固定音声・モデル・実行条件の下で検証するための入口です。認識精度、受信欄で確認した挿入、誤入力を防いだガードの結果をそれぞれ記録します。実行ごとに新しいローカルディレクトリを使います。

## 実行する

| プロファイル | smoke | full |
| --- | --- | --- |
| `headless` | mock 認識、合成 PCM の実時間リプレイ、補正採点器の固定フィクスチャ | 同左 |
| `cuda` | ホスト上の ASR ワーカーで smoke 60件を各1回 | smoke 60件と holdout 200件を各2回 |
| `vm` | mock 挿入2ケース、ガード2ケース、実モデルのメモ帳挿入1回 | mock 挿入6ケース、ガードと障害回復5ケース、実モデルはメモ帳6回（短音声では10回）・VS Code 2回・Chrome 2回 |
| `all` | 上記3プロファイルの smoke | 上記3プロファイルの full |

ヘッドレス評価は GitHub CI でも実行します。VM や CUDA の実行は、環境を準備したうえでプロファイルを明示します。

### Phase 1 の基準計測を再現する

初回準備を済ませ、ホスト専用の `.ai/evaluation.local.json` を設定してから実行します。スキーマは `asr-evaluation-settings/1` です。 `model_dir`、`app_binary`、`app_revision`、`credential` を設定し、既定の `bench_data/manifests` 以外を使う場合は `manifest_dir` を追加します。

```powershell
uv run --offline --no-sync scripts/evaluate.py --suite full --profile all --vm-fixture short
```

基準計測はホストの CUDA 認識と、実モデルを CPU / `int8` で動かす隔離 VM の挿入を続けて実行します。VM は日本語、トグル録音、補正オフの評価プロファイルを使います。固定音声は CUDA の smoke 60件と holdout 200件を各2回、VM では3.19秒の同じ音声をメモ帳10回、VS Code と Chrome で各2回使います。VM は挿入先を切り替えるたびに製品を新しく起動し、同じ挿入先の連続試行では製品、ワーカー、モデルを使い続けます。停止から挿入までの warm 集計にはメモ帳の2〜10回目を使います。

1. 対象の Git リビジョンで Windows 用のリリース実行ファイルを用意します。ソースからビルドする場合は、Node.js 22、`package.json` 指定の pnpm、Rust、Windows のビルド依存をそろえ、`pnpm install --frozen-lockfile` と `pnpm exec tauri build --no-bundle` を実行します。依存の初回準備は `uv sync --locked --python 3.12.11` で行います。ビルド元リビジョンは実行者が指定し、ランナーは実際に使うファイルの SHA-256 を照合・記録します。[開発環境](../README.md) · [CI のビルド手順](../.github/workflows/ci.yml)
2. 音声の利用条件を確認し、FLEURS と JVNV のアーカイブを用意します。次のコマンドで、同名のマニフェストと音声ディレクトリがまだないデータルートに復元します。別のデータルートを使う場合は、そこに元アーカイブと取得記録をそろえます。出力の `manifests` を `--manifest-dir` に指定できます。[データの取得と照合](eval_set/public_datasets.md)

   ```powershell
   $data = 'bench_data'
   $sets = 'practical-ja-smoke-v1', 'practical-ja-holdout-v1', 'practical-ja-vm-latency-v1'
   foreach ($set in $sets) {
       uv run --offline --no-sync scripts/bench_data.py build --cache-dir $data --lock "docs/eval_set/public/$set.lock.json" --out $set
       if ($LASTEXITCODE -ne 0) { throw "Dataset build failed for $set" }
   }
   ```

3. ホストに `faster-whisper-large-v3-turbo` の固定スナップショットと CTranslate2 が使う CUDA ランタイムを用意します。VM は下記の復旧手順に従って準備します。計測時は GPU を使う他のジョブを避け、差分を判断する際は機器、ドライバー、負荷条件を照合します。
4. コマンドを実行し、`run.json`、`summary.json`、`summary.md`、そこから参照される各ステージの証跡を同じ出力ルートに保存します。比較する変更は新しい出力先で計測します。再開は元の引数に `--output <元の出力ルート> --resume` を加えます。

### 隔離 VM を準備・復旧する

1. ホストに Hyper-V と PowerShell 7 を用意します。Hyper-V の VM 名と Windows 11 ゲストのコンピューター名をともに `ASR-Eval` に設定します。一括実行には専用の標準ユーザー `ASREvalReal` で対話サインインします。ゲスト管理者の資格情報は PowerShell Direct 用に、実行するホストユーザーで `Export-Clixml` して保管します。資格情報は成果物に含めません。[PowerShell Direct の前提と接続方法](https://learn.microsoft.com/en-us/windows-server/virtualization/hyper-v/powershell-direct)
2. ゲストで署名を確認した VB-CABLE を導入し、`CABLE Input` と `CABLE Output` が有効であること、デスクトップアプリがマイクを利用できることを確認します。署名を確認した Chrome と VS Code、Windows のメモ帳・Edge・Terminal を下記の VM 試験と同じ配置で用意します。メモ帳は新しいセッションで起動する設定にして、評価前にすべて閉じます。[VB-CABLE の配布元](https://vb-audio.com/Cable/)
3. ゲストの `C:\ASR-Evaluation\runtime` に `uv.exe` と `python\python.exe` を配置し、`real-deps\site-packages` に同じ Python ABI の依存ライブラリを用意します。固定モデルは `C:\ASR-Evaluation\models\faster-whisper-0a363e9161cbc7ed1431c9597a8ceaf0c4f78fcf` に配置します。ランナーは試験用設定 DB がなければ作ります。設定不一致のときはゲストの製品を終了し、評価ユーザーの `%APPDATA%\org.localvoice.input\local-voice-input.sqlite3` と付随ファイルを別名で退避してから再実行します。残留タスクがある場合は「結果を読む」の所有確認を先に行います。[ゲストの実モデル構成](windows_evaluation.md#実モデルでのメモ帳挿入)
4. 復旧後の VM 確認には、本番のフル計測と同じ設定で `uv run --offline --no-sync scripts/evaluate.py --suite full --profile vm --vm-fixture short` を使います。両ステージの合格を確認すると、Chrome・VS Code、短音声、ガードを含む復旧確認になります。既定の `--vm-mode both` は `ASREvalReal` のまま mock と実モデルを実行します。従来の mock 単独モードも使う場合は別の標準ユーザー `ASREval` を用意します。

### 判定基準

| 対象 | Phase 1 の合格条件 |
| --- | --- |
| ヘッドレス3ステージ | 合成音声の認識・実時間リプレイ・補正採点器がそれぞれ固定フィクスチャの期待値に一致する |
| CUDA 2ステージ | smoke 60件・holdout 200件が各2回計測され、全リクエストが実測で CUDA / `int8_float16`。全発話を採点できる |
| VM の通常挿入 | 固定文6/6、実モデル14/14で独立した受信側が期待文字列と全文一致する。実モデルの期待文字列はワーカー出力。製品が `paste_unverified` を報告した場合も受信側で検証する |
| VM のガード・回復 | 4ガードとワーカー障害回復がすべて合格する。ガードで禁止された変更がないこと、障害回復では元ワーカーの終了後に同じアプリの次の試行が成功することを確認する |

選択した7ステージのすべてが `PASS` のとき、基準計測は合格です。前提や証跡の不足は `BLOCKED`、実行されなかった選択済みの試験は `NOT_RUN` として残します。通常挿入の分母には予定した全件を含め、`clipboard_only`、タイムアウト、受信内容の不一致、未確認は成功に数えません。ガードの判定と認識の採点は通常挿入率と分けます。

CUDA の raw / normalized CER は各発話の最初の warm 出力を採点し、編集距離の総和を正解文字数の総和で割ります。ワーカー往復時間の p50 / p95 は2回分の warm リクエストを集計します。VM はゲスト CPU の試行 CER の中央値と、停止操作から独立した受信確認までの時間を記録し、メモ帳 warm の観測件数と予定件数を併記します。この遅延には、ワーカー側の証跡生成と受信側のポーリング待ちが含まれます。変更前後の比較ではデータ、測定区間、実行条件をそろえ、CER や遅延の増加は再計測して確認します。7ステージの `PASS` は計測とガードの成立を示します。数値の回帰や #111 の製品目標は、計測の処理負荷と観測間隔を別途評価できる測定で判定します。

### 2026-11-25 の基準値

フル評価は7/7ステージが合格しました。評価ソースはコミット `c3ecc0dc1088363f175907dc1f5e11b97cdcc785` と照合済みです。再開時は最初に合格した5ステージの証跡を検証して再利用し、VM の2ステージを実行しました。ホストの GPU は RTX 3060 12 GiB、NVIDIA ドライバーは 621.40 でした。ゲストは Windows 11 Pro 26300.9974、4 vCPU、起動時8 GiB（動的メモリ有効）、VB-CABLE 3.5.2.0 です。ゲストの Python は3.12.11、uv は0.8.8、faster-whisper は1.2.1、CTranslate2 は4.8.0でした。

| 測定 | 件数 | 結果 |
| --- | --- | --- |
| CUDA smoke | 60発話・120要求 | raw CER 11.50%、normalized CER 5.72%、ワーカー往復 p50 / p95 427 / 554 ms |
| CUDA holdout | 200発話・400要求 | raw CER 10.61%、normalized CER 4.85%、ワーカー往復 p50 / p95 422 / 575 ms |
| VM 固定文・ガード | 通常挿入6件・ガード4件・障害回復1件 | 11件すべて合格 |
| VM 実モデル | メモ帳10件・VS Code 2件・Chrome 2件 | 確認済み挿入14/14。試行 CER 中央値は raw 16.67%、normalized 0.00%。メモ帳 warm の停止から挿入までの p50 / p95 は 10.70 / 11.58 秒（9/9件、ゲスト CPU） |

基準レポートはローカルの `.ai/evaluation/phase1-merge-028fd8765cc945a3b9d590959b90183c/report-71efcc53393644e8a98c162557f50703` にあります。同じ実行ルートに、分母・モデル実測・証跡ハッシュを照合した `verification.json` と、評価入力をハッシュ付きで保存した `reproduction-inputs.zip` があります。再利用時は計測コミットの別チェックアウトに ZIP の `source/` を展開し、`product/local-voice-input.exe` を設定の `app_binary` に指定します。ZIP 内の `index.json` でハッシュを照合できます。

固定音声は元アーカイブから別のデータルートへ261件を復元し、通常の事前確認で照合しました。ゲスト用の Python と依存ライブラリもホストの別ディレクトリに展開し、復元した短音声を製品ワーカーと同じ認識経路でオフライン認識できることを確認しました。CPU の設定は `int8`、CTranslate2 の実測値は `int8_float32` でした。使用したアーカイブと復元の証跡は[基準計測記録](tasks/issue-110-final-baseline.md)にあります。Phase 1 のフル評価と完了判定は、準備済みの隔離 VM を対象とします。

### 通常の実行

```powershell
uv run --offline --no-sync scripts/evaluate.py
```

結果は各実行で新しい `.ai/evaluation/` 配下に保存されます。ローカル設定の相対パスはリポジトリから解決し、コマンドの明示引数で上書きできます。`credential` に指定するのは暗号化された資格情報ファイルへのパスです。

```powershell
uv run --offline --no-sync scripts/evaluate.py --profile all
```

CUDA では `--model-dir` に固定リビジョン `0a363e9161cbc7ed1431c9597a8ceaf0c4f78fcf` のモデルディレクトリを指定します。事前に各音声と正解文、モデル5ファイルのハッシュを照合します。ベンチマーク時のデコード条件と実際の CUDA / int8_float16 を確認してから集計します。CUDA と実モデル VM が参照するマニフェストは既定で `bench_data/manifests` に置き、変更する場合は `--manifest-dir` を指定します。[固定データの復元手順](eval_set/public_datasets.md#lock-の保存と別pcでの再構築)

VM では `--app-binary`、ビルド元の `--app-revision`、PowerShell Direct 資格情報ファイルの `--credential` を設定します。既定の `--vm-mode both` では、標準ユーザー `ASREvalReal` をサインインさせてから実行します。mock 試験で使うワーカーは VM に配置した試験コピーで差し替え、同じセッションで実モデル試験へ進みます。`--vm-mode real` も `ASREvalReal`、従来の mock 専用実行 `--vm-mode mock` は `ASREval` を使います。[VM の初期準備と実行条件](windows_evaluation.md#110-の-vm-内挿入スモーク)

両評価ユーザーのメモ帳を新しいセッションで起動する設定にし、実行前にすべてのメモ帳ウィンドウを閉じます。開いているウィンドウがある場合は、その試行を `BLOCKED` として記録します。

### 挿入先とガードを広げる

```powershell
uv run --offline --no-sync scripts/evaluate.py --suite full --profile vm
```

対象は隔離 VM 内の試験専用ウィンドウです。ネイティブの入力欄、メモ帳、Edge、VS Code、Chrome、Windows Terminal で固定文の挿入を確認します。Chrome と Edge は一時プロファイルのローカルページ、VS Code は一時プロファイルの空ファイル、Windows Terminal は専用の文字列受信プログラムを使います。ゲストの `C:\ASR-Evaluation\tools\VSCode` に VS Code ZIP 版を展開し、`C:\Program Files\Google\Chrome\Application\chrome.exe` に公式 Windows x64 版の Chrome を用意します。準備時に実行ファイルの署名を確認します。Edge と Windows Terminal はゲストの既存アプリを使います。[VS Code の ZIP 版](https://code.visualstudio.com/docs/setup/windows) · [Chrome のインストーラー](https://support.google.com/chrome/a/answer/7650032)

ガードは、フォーカス移動、録音中のキャンセル、選択変更、フォーカス移動と最小化の組み合わせを試します。選択変更と最小化では操作の成立も確認し、挿入先と移動先の文字列が変わらなかった場合に成功とします。端末などで製品が `paste_unverified` を報告した場合は、受信側で全文一致を確認できたときに挿入成功に数え、製品の報告も表示します。

障害回復では、試験用の認識ワーカーを一度だけ異常終了させます。失敗した試行で両方の入力欄が変わらず、同じアプリを起動したまま新しいワーカープロセスで次の挿入が成功したことを確認します。クラッシュ前後の試行は通常挿入の成功率と別に集計します。

mock ではケースごとに識別子を変え、以前の試験で残った文字列の貼り付けを検出します。実モデルはメモ帳、VS Code、Chrome で同じ固定音声を使用します。アプリごとに受信内容と認識ワーカーの出力が一致した件数を表示し、参照テキストに対する CER は別欄で報告します。同じ認識結果がすでにクリップボードにある場合、実モデル側の全文照合では古い内容の貼り付けを識別できません。クリップボード競合の検証は #111 の専用シナリオで扱います。実モデルだけを短く確認するには `--vm-mode real --vm-fixture short --vm-repeats 2` を追加します。

### 3秒級の挿入遅延

既存の JVNV アーカイブから、holdout の話者を除いた開発用の regular 発話のうち、3秒に最も近い1件を選びました。3.19秒のこの日本語発話は、選択済みの項目を記録した[固定 lock](eval_set/public/practical-ja-vm-latency-v1.lock.json)で復元します。

```powershell
uv run --offline --no-sync scripts/bench_data.py build --cache-dir bench_data --lock docs/eval_set/public/practical-ja-vm-latency-v1.lock.json --out practical-ja-vm-latency-v1
```

実モデル用の VM 引数に `--suite full --profile vm --vm-mode real --vm-fixture short` を指定すると、メモ帳で初回1回と warm 9回を測り、VS Code と Chrome で各2回の挿入を確認します。停止からメモ帳で全文の一致を確認するまでの p50 と p95 は、挿入を確認できたメモ帳の warm 試行から算出し、予定件数と観測件数を併記します。同じ1発話の少数回の測定なので、Phase 2 の目標値の合否には使いません。`--vm-fixture original` では従来の9.3秒の入力を使います。

連続試験では `--vm-repeats` でメモ帳の試行数を2〜80回に指定できます。各アプリの連続試行でアプリ、認識ワーカー、モデルのロード識別子が同じままかを確認し、毎回新しい入力欄を使います。

## 結果を読む

- 標準出力は全体の結果と `summary.md` の場所です。`--verbose` で進捗を表示できます。Markdown は選んだ試験の合否、主要指標、失敗理由と証跡への参照をまとめます。入力のハッシュ、Git リビジョン、実行環境、対象外の試験は `summary.json` に入ります。通常挿入の分母には予定した全件を使い、ガードの正常な拒否は別に表示します。
- `PASS` は対象ステージの計測・検証完了です。製品の性能目標は、測定値とサンプル数を見て判定します。`FAIL` は確認した検証条件の不成立、`BLOCKED` は前提や証跡の不足です。終了コードは順に 0・1・2、中断時は 130 です。対象外のステージは `NOT_SELECTED` として残ります。
- `--resume` は同じ構成と評価ソースで実行します。入力と元の証跡の SHA-256 が一致する合格ステージを再利用し、その他は新しい試行ディレクトリで実行します。既存の試行やレポートは保持します。VM 実行中の Ctrl+C では停止要求を渡し、ホストランナーの後処理を待ちます。後処理を確認できない場合は `BLOCKED` とし、後続の VM 試験を保留します。
- `cleanup_error` とタスクの残留が報告された場合は、ゲストで `ASR-Eval-InsertionSmoke` の実行状態と所有ランを確認してください。該当ランのアプリとフィクスチャの終了を確認してからタスクを解除すると再開できます。コントローラーは所有を確認できないタスクには触れません。
- VM 試行がタイムアウトした場合は後続の VM 評価を停止します。ゲストの評価ユーザーをサインアウトしてから再度サインインし、キーとフォーカスの状態をリセットします。ゲストのランタイム、入力先アプリ、音声デバイスを変更した場合は、新しい出力先で全対象を実行します。
- 生の認識結果を含む ASR ワーカー出力はローカル保存です。CI は合成音声と固定フィクスチャを使い、実モデルの結果や VM 資格情報をアップロードしません。

## 後続 Issue で使う測定

| 後続 Issue | 現在使える基準 | 追加の評価 |
| --- | --- | --- |
| [#111 入力・録音・推論の安定化](https://github.com/CS-Kmkm/asr/issues/111) | 6種の mock 挿入、4ガード、ワーカー異常終了からの回復、3アプリでの実モデル挿入と固定 CUDA 認識 | 実アプリ上の IME・クリップボード競合・処理中キャンセル、代表音声による成功率、リソースや残留プロセスを含む長時間安定性 |
| [#112 操作と文章品質](https://github.com/CS-Kmkm/asr/issues/112) | 録音準備／停止から挿入までの時間、意味・数字・否定などの補正採点器 | 操作タスクの成功率・回復率、実製品の補正や設定変更を含む E2E |
| [#113 独立データでの研究評価](https://github.com/CS-Kmkm/asr/issues/113) | lock 付きの dev / holdout、CER と処理時間、モデル・音声の同定 | 囁きと通常発話を区別した独立セット、手法別の比較条件と採用基準 |

#111 の暫定目標は通常入力の確認済み成功率 99.5% 以上、禁止された変更0件、3秒 warm の停止から挿入まで p50 1.5秒未満・p95 2.5秒未満です。達成判定の前に、対象アプリ別の試行数、代表音声・操作、95%信頼区間の出し方、製品を通す測定機器と実行条件を固定します。Phase 1 の14回は回帰確認用です。99.5%の判定に必要な試行は #111 の評価で追加します。ゲスト CPU 上の製品挿入とホスト CUDA のワーカー往復はそれぞれ別の基準値として使い、製品の CUDA 経路に対する停止から挿入までの判定には、その製品経路の E2E 計測を用います。[#111 の受け入れ条件](https://github.com/CS-Kmkm/asr/issues/111)
