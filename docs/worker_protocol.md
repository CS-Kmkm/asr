# ASRワーカー JSONLプロトコル仕様

本ドキュメントは、Rust側クライアント (`src-tauri/src/asr.rs` の `JsonlTranscriber`) と
Pythonサイドカーワーカー (`asr_worker/worker.py`) が交わすJSON Linesプロトコルを規定する。
v1(現行)仕様は実装と1対1で対応し、v2拡張提案は未実装である旨を明記して隔離する。

関連文書:

- 実装計画書 `docs/local_ai_voice_input_implementation_plan.md` §7.2(主要インターフェース)
- ADR-0002 `docs/adr/0002-batch-asr-v1.md`(録音停止後の一括認識)
- 要件 R-006(フローティングUI、Escキャンセル要件)

---

## 1. 概要

### 1.1 トランスポート

- 通信路はワーカープロセスの **stdin / stdout** である。stderrは診断ログ専用であり、
  プロトコルメッセージを含まない。
- **1行に1メッセージ**(JSON Lines)。リクエスト・レスポンスはいずれも単一のJSONオブジェクトで、
  末尾に改行 (`\n`) を付与して送出する。
- 文字エンコーディングは **UTF-8** である。ワーカーは `ensure_ascii=False` で応答を直列化するため、
  非ASCII文字はエスケープせずそのまま出力される。
- ワーカーの応答はコンパクト形式(区切りに余分な空白を含まない)で出力される。
- 1リクエストにつき必ず1レスポンスが返る、同期的な request/response 往復モデルである
  (ADR-0002 のbatch方式に対応)。
- Rust側はレスポンス1行のサイズに上限 (`MAX_RESPONSE_BYTES` = 8 MiB) を設けており、
  これを超える行はプロトコルエラーとして扱う。

### 1.2 プロセスライフサイクル

- Rust側がワーカーを **spawn** する。起動コマンドは既定で `python -m asr_worker` であり、
  `--backend` 引数または環境変数 `ASR_WORKER_BACKEND` でバックエンド
  (`vibevoice` / `faster-whisper` / `mock`、既定 `vibevoice`)を選択する。
- ワーカーはstdinがEOFに達するか `shutdown` を受信するまでメッセージを処理し続ける。
- ワーカープロセスは **ステートフル**である。`load` 後に内部状態 `loaded = True` を保持し、
  `transcribe` はロード済みを前提とする。
- **ロード状態の復元はRust側の責務である。** ワーカーがクラッシュ・タイムアウト・
  プロトコル違反等で再spawnされた場合、ワーカー自身は前回のロード状態を記憶しない。
  Rust側 (`JsonlTranscriber`) が最後に成功した `load` の量子化設定 (`loaded_quantization`) を
  保持しており、再spawn時 (`ensure_running`) に自動的に `load` を再送してモデルロード状態を
  再現する。ワーカープロトコルにはハンドシェイクや状態問い合わせの仕組みは存在しない。

---

## 2. プロトコル v1(現行)

### 2.1 共通事項

- すべてのリクエストは `id` フィールドを **必須**とする。`id` を欠くリクエストは
  `invalid_request` エラーになる。
- レスポンスはリクエストと同じ `id` をそのまま反映(エコーバック)する。Rust側は
  レスポンスの `id` がリクエストの `id` と一致することを検証し、不一致はプロトコルエラーとする。
- `id` の値の型はワーカー側では制約されない(数値・文字列いずれも許容され、テストでも双方が使われる)。
  ただしRust側クライアントは単調増加する整数 (`AtomicU64`, 初期値1) を採番する。
- 操作の指定キーは `command` を正とするが、後方互換のため `op` も受理する
  (`request.get("command", request.get("op"))`)。Rust側は常に `command` を送出する。
- 成功レスポンスは `"ok": true` を、失敗レスポンスは `"ok": false` と `error` オブジェクトを含む。
- リクエストやレスポンスに未知のフィールドが含まれても無視される(Rust側の型は必要フィールドのみを参照)。

エラー共通形式:

```json
{"id": 7, "ok": false, "error": {"code": "audio_not_found", "message": "..."}}
```

`id` が判定できない場合(JSON不正、JSONオブジェクトでない、`id` 欠落)、`error` の `id` は
`null` になる。

---

### 2.2 load 操作

モデルを指定の量子化設定でロードする。

**リクエストフィールド:**

| フィールド | 型 | 必須 | 説明 |
|---|---|---|---|
| `id` | 任意(数値/文字列) | 必須 | リクエスト識別子。欠落で `invalid_request`。 |
| `command` | `"load"` | 必須 | 操作名(`op` でも可)。 |
| `quantization` | 文字列 | 任意 | 量子化設定。既定値 `"4bit"`。文字列以外は `invalid_request`。 |

`quantization` に許容される値はバックエンドのロード時に検証される。共通の許容集合は
`{"4bit", "8bit", "bf16"}` であり、これ以外は `unsupported_quantization` になる。

**成功レスポンスフィールド:**

| フィールド | 型 | 説明 |
|---|---|---|
| `id` | リクエストと同一 | エコーバック。 |
| `ok` | `true` | 成功。 |
| `model` | 文字列 | バックエンドのモデル名。バックエンドの `model_name`(後述)。 |
| `quantization` | 文字列 | 要求された量子化設定をそのまま返す。 |

`model` の値はバックエンドにより異なる:

- VibeVoice: `"microsoft/VibeVoice-ASR-HF"`
- faster-whisper: `"faster-whisper:<モデルID>"`(既定モデルID `large-v3-turbo`、
  環境変数 `ASR_FASTER_WHISPER_MODEL` で変更可)
- mock: `"microsoft/VibeVoice-ASR-HF:mock"`

**JSON例:**

```json
→ {"id": 1, "command": "load", "quantization": "4bit"}
← {"id": 1, "ok": true, "model": "microsoft/VibeVoice-ASR-HF", "quantization": "4bit"}
```

`quantization` 省略時は `"4bit"` が適用される:

```json
→ {"id": 1, "command": "load"}
← {"id": 1, "ok": true, "model": "...", "quantization": "4bit"}
```

ロードが成功すると、以降ワーカーは `transcribe` を受理できる状態になる。

---

### 2.3 transcribe 操作

ロード済みモデルで音声ファイルを認識する。

**リクエストフィールド:**

| フィールド | 型 | 必須 | 説明 |
|---|---|---|---|
| `id` | 任意 | 必須 | リクエスト識別子。 |
| `command` | `"transcribe"` | 必須 | 操作名(`op` でも可)。 |
| `audio_path` | 文字列(非空) | 必須 | 音声ファイルのパス。非空文字列でなければ `invalid_audio_path`、ファイルが存在しなければ `audio_not_found`。 |
| `prompt` | 文字列 / 文字列リスト / `null` / 省略 | 任意 | 認識ヒント(辞書語など)。詳細は下記。 |

**`prompt` の扱い:**

- **文字列**: そのままバックエンドに渡される。
- **文字列リスト**: 辞書語の配列とみなす。全要素が文字列であることを検証し
  (非文字列要素があれば `invalid_request`)、各要素を `strip()` した上で空要素を除外し、
  改行 (`\n`) で連結して単一文字列に変換してから渡す。
- **`null` または省略**: ヒントなし。
- 上記以外の型(数値・オブジェクト等)は `invalid_request`。

バックエンドごとの `prompt` の利用:

- VibeVoice: 認識リクエストの `prompt` として処理に渡される。
- faster-whisper: 非空のときのみ `hotwords` として渡される(空文字列は `hotwords=None` 扱い)。

**成功レスポンスフィールド:**

| フィールド | 型 | 説明 |
|---|---|---|
| `id` | リクエストと同一 | エコーバック。 |
| `ok` | `true` | 成功。 |
| `text` | 文字列 | 認識テキスト全文。 |
| `segments` | オブジェクトの配列 | セグメント配列(下記形状)。 |
| `model` | 文字列 | バックエンドのモデル名。 |
| `duration_ms` | 整数 | ワーカー内での認識所要時間(ミリ秒、`round` 済み)。`load` を含まず `transcribe` 呼び出しのみを計測。 |

**`segments` の各要素の形状:**

| フィールド | 型 | 説明 |
|---|---|---|
| `start` | 数値(float) | セグメント開始秒。 |
| `end` | 数値(float) | セグメント終了秒。 |
| `speaker` | 任意 / `null` | 話者識別子。VibeVoiceは話者番号等を返しうる。faster-whisperおよびエラーフォールバックでは `null`。Rust側では `Option<Value>` として保持され、型を限定しない。 |
| `text` | 文字列 | セグメントのテキスト。 |

`text` 全文の組み立て規則はバックエンドにより異なる(VibeVoiceは各セグメントを半角空白で連結、
faster-whisperは空文字で連結し前後を `strip()`)が、いずれもレスポンスでは確定済みの
`text` フィールドとして返る。

**JSON例:**

```json
→ {"id": 2, "command": "transcribe", "audio_path": "/tmp/utt.wav", "prompt": "VibeVoice\nTauri"}
← {"id": 2, "ok": true, "text": "hello world", "segments": [{"start": 0.0, "end": 1.5, "speaker": 0, "text": "hello world"}], "model": "microsoft/VibeVoice-ASR-HF", "duration_ms": 842}
```

`prompt` を辞書語リストで渡す例:

```json
→ {"id": 2, "command": "transcribe", "audio_path": "/tmp/utt.wav", "prompt": ["VibeVoice", "Tauri"]}
← {"id": 2, "ok": true, "text": "...", "segments": [...], "model": "...", "duration_ms": 720}
```

ロード前に呼び出した場合:

```json
→ {"id": 2, "command": "transcribe", "audio_path": "/tmp/utt.wav"}
← {"id": 2, "ok": false, "error": {"code": "model_not_loaded", "message": "Load the model before transcription"}}
```

---

### 2.4 shutdown 操作

ワーカーに正常終了を指示する。

**リクエストフィールド:**

| フィールド | 型 | 必須 | 説明 |
|---|---|---|---|
| `id` | 任意 | 必須 | リクエスト識別子。 |
| `command` | `"shutdown"` | 必須 | 操作名(`op` でも可)。 |

**成功レスポンスフィールド:**

| フィールド | 型 | 説明 |
|---|---|---|
| `id` | リクエストと同一 | エコーバック。 |
| `ok` | `true` | 成功。 |

`shutdown` のレスポンスを返した後、ワーカーは `serve` ループを終了し、プロセスは正常終了する。

**JSON例:**

```json
→ {"id": 3, "command": "shutdown"}
← {"id": 3, "ok": true}
```

---

## 3. エラーコード一覧

`error.code` は実装上の文字列定数であり、以下が全コードである。`message` は人間可読の
英語文字列で、コードに対する補助情報として扱う(Rust側はコードを正とし `message` を解釈しない)。

| コード | 発生箇所 | 発生条件 |
|---|---|---|
| `invalid_request` | worker.py | リクエストがJSONオブジェクトでない / `id` が欠落 / `quantization` が文字列でない / `prompt` の型が不正(非文字列要素を含むリスト、文字列・リスト・null以外)。 |
| `invalid_json` | worker.py (`serve`) | 受信行がJSONとして解釈できない(`JSONDecodeError`)。`id` は `null`。 |
| `model_not_loaded` | worker.py | `load` 成功前に `transcribe` を要求した。 |
| `invalid_audio_path` | worker.py | `audio_path` が非空文字列でない。 |
| `audio_not_found` | worker.py | `audio_path` が示すパスが存在しない、またはファイルでない。 |
| `unsupported_operation` | worker.py | `command`/`op` が `load`/`transcribe`/`shutdown` のいずれでもない。 |
| `unsupported_quantization` | backends.py(各バックエンドの `load` / `faster_whisper_compute_type`) | `quantization` が許容集合 `{4bit, 8bit, bf16}` 外。 |
| `unsupported_backend` | backends.py (`create_backend`) | 未知のバックエンド名。プロセス起動時 (`main`) に発生し、終了コード2でワーカーが起動失敗する(この場合プロトコル応答ではなくstderr出力)。 |
| `backend_unavailable` | backends.py (`FasterWhisperBackend.load`) | `faster_whisper` パッケージが未インストール(ImportError)。 |
| `gpu_oom` | backends.py(例外マッピング / mock) | 例外メッセージに "out of memory" または "cuda oom" を含む(load/transcribe両方で発生しうる)。 |
| `gpu_unsupported` | backends.py (`VibeVoiceBackend.load`) | CUDA GPUが利用不可、またはモデルがCUDAに配置されなかった(CPUフォールバック無効)。 |
| `model_load_failed` | backends.py(例外マッピング) | `load` 中の予期しない例外(OOM以外)。 |
| `transcription_failed` | backends.py(例外マッピング) | `transcribe` 中の予期しない例外(OOM以外)。 |
| `internal_error` | worker.py | 上記のいずれにも該当しない予期しない例外を `handle` が捕捉した場合の包括フォールバック。 |

補足:

- `unsupported_backend` は起動時のバックエンド生成失敗としてのみ発生し、ワーカーは
  終了コード2で即時終了する。これはプロトコル上のレスポンスではなく、起動時の失敗である。
- `BackendError` として送出されたコード(`unsupported_quantization`, `backend_unavailable`,
  `gpu_oom`, `gpu_unsupported`, `model_load_failed`, `transcription_failed`)は
  `handle` 内で捕捉され、そのコードのままレスポンスに反映される。
- それ以外の想定外例外はすべて `internal_error` に丸められ、詳細はstderrにのみ出力される
  (`message` には例外の内部詳細を含めない)。

---

## 4. クライアント側の挙動規約(Rust)

`JsonlTranscriber` の挙動。プロトコル仕様そのものではないが、ワーカー実装者が前提とすべき
クライアント契約である。

### 4.1 タイムアウト

- 各リクエスト・レスポンス往復には設定可能なタイムアウトが適用される
  (`request_timeout`、`src-tauri/src/lib.rs` での現行設定値は **300秒**)。
- レスポンスが時間内に得られない場合 `AsrError::Timeout` となる。
- `shutdown` ではレスポンス交換とは別に、プロセス終了の待機にも同じタイムアウトを適用する。
  時間内に終了しない場合はプロセスをkillする。

### 4.2 リセット条件(再spawnの引き金)

`requires_reset` が真となるエラーが発生すると、Rust側は実行中ワーカーをkillして破棄する
(`reset_locked`)。次のリクエスト時に `ensure_running` が再spawnし、保持している
`loaded_quantization` があれば `load` を再送してロード状態を復元する。

リセットを引き起こすエラー:

- `AsrError::Io` — stdin/stdout のI/O失敗。
- `AsrError::Protocol` — レスポンスがJSONでない / `id` 不一致 / サイズ超過 /
  成功レスポンスを `Transcript` にデシリアライズできない。
- `AsrError::Crashed` — レスポンス読み取りでEOF(ワーカーが応答なしに終了)。
- `AsrError::Timeout` — 上記タイムアウト。
- `AsrError::Cancelled` — キャンセル信号により進行中リクエストを放棄(後述)。

一方、`AsrError::Worker`(`ok:false` のワーカー報告エラー)は **リセットを引き起こさない**。
ワーカープロセスは正常に応答しており、再利用可能とみなす。

### 4.3 キャンセル(現行の実装)

- `transcribe` は `watch::Receiver<bool>` のキャンセル信号を受け取る。
- 信号が立つと、Rust側は進行中の `exchange`(ワーカー応答待ち)を `tokio::select!` で放棄し、
  `AsrError::Cancelled` を返す。これは 4.2 によりワーカーのkill・再spawn・モデル再ロードを
  引き起こす。
- **ワーカープロトコル自体にcancel操作は存在しない。** 進行中の `transcribe` を
  ワーカー側で中断する手段はなく、放棄されたリクエストの認識処理はワーカープロセスの
  終了(kill)によって停止する。これがR-006のEscキャンセル要件に対する現行の実現手段である。

### 4.4 shutdownの待機

- `shutdown` 送信後、Rust側はレスポンス交換に続けてプロセス終了 (`child.wait()`) を
  タイムアウト付きで待機する。
- 時間内に終了しない場合はkillしてから再度 `wait` する。

---

## 5. v2拡張提案(未実装)

> 本節の内容は **すべて未実装の提案**である。現行 (v1) のワーカーおよびRustクライアントは
> いずれの機能も備えていない。設計レビューで指摘された3点
> (バージョンハンドシェイクなし / 進捗イベントなし / キャンセル不能)への対応案を記す。

### 5.1 バージョンハンドシェイク(hello / capabilities)

**課題:** 現行プロトコルにはバージョン折衝がなく、Rustクライアントとワーカーの
プロトコル不整合を検出できない。新フィールドや新操作を追加した際の互換管理ができない。

**提案:** spawn直後の最初の交換として `hello` を導入する。

```json
→ {"id": 0, "command": "hello", "client_protocol": 2}
← {"id": 0, "ok": true, "protocol": 2, "backend": "vibevoice",
   "capabilities": ["transcribe", "progress", "cancel"]}
```

- ワーカーは自身が話せる最大プロトコルバージョンと、対応する操作・拡張機能の集合
  (`capabilities`)を返す。
- Rustクライアントは応答の `protocol` と `capabilities` を見て、利用可能な機能のみを使う。
  例えば `progress` や `cancel` が含まれなければ現行 (v1) の挙動にフォールバックする。
- v1ワーカーは `hello` を未知の操作として `unsupported_operation` を返すため、
  クライアントは「v1ワーカーである」と判別でき、後方互換を保てる。

### 5.2 transcribe の進捗イベント

**課題:** R-006 は処理中に「ASR中」等の段階表示を求めるが、現行の `transcribe` は
処理完了まで単一の最終レスポンスしか返さず、進捗を可視化できない。

**提案:** `transcribe` に対し、最終レスポンスの前に **同一 `id` の進捗イベントを複数行**
送出する。最終行のみ `ok` を含み、進捗行は `event: "progress"` で区別する。

```json
→ {"id": 5, "command": "transcribe", "audio_path": "/tmp/utt.wav"}
← {"id": 5, "event": "progress", "stage": "preprocess"}
← {"id": 5, "event": "progress", "stage": "decode", "ratio": 0.4}
← {"id": 5, "ok": true, "text": "...", "segments": [...], "model": "...", "duration_ms": 842}
```

- `stage` は処理段階(例 `preprocess` / `decode` / `postprocess`)を示す列挙文字列。
- `ratio` は任意の進捗率(0.0〜1.0)。モデルによっては算出困難なため任意とする。
- クライアントは `ok` も `error` も持たない行を中間イベントとして扱い、最終行で完了とみなす。
  この設計は1リクエスト1レスポンスの不変条件を崩すため、`capabilities` に `progress` が
  含まれる場合のみ有効化する(5.1と併用)。
- 注意: VibeVoice等は `model.generate(...)` がブロッキングであり、トークン単位の
  進捗取得には `streamer` 等の組み込みが必要になる。段階粒度の進捗(前処理/生成/後処理)は
  比較的容易だが、生成中の細粒度進捗は追加実装を要する。

### 5.3 cancel 操作の設計案

R-006 はEscキャンセルを要求する。実装計画書 §R-006 設計注記の通り、現行はkill+再spawn+
再ロードで代替している。v2での恒久対応として2案を比較する。

#### 案A: プロトコルに cancel 操作を追加

進行中の `transcribe` を識別子で指す `cancel` をワーカーに送る。

```json
→ {"id": 5, "command": "transcribe", "audio_path": "..."}
→ {"id": 6, "command": "cancel", "target_id": 5}
← {"id": 6, "ok": true}            (キャンセル受理)
← {"id": 5, "ok": false, "error": {"code": "cancelled", "message": "..."}}
```

- **利点:** モデルを再ロードせずに済むため、次の認識をすぐ開始できる。プロセスの再起動が不要。
- **欠点 / 技術的困難:**
  - 単一スレッドの同期的 `serve` ループでは、`transcribe` 処理中に後続行 (`cancel`) を
    読めない。読み取りと処理を別スレッド化し、`generate` 中に割り込む仕組みが必要になる。
  - 多くのモデル実装で `model.generate(...)` は協調的中断点を持たない。
    Hugging Faceの `StoppingCriteria` を使えば次トークン境界で停止できるが、
    バックエンド非依存ではない。faster-whisper等は中断APIの提供状況が異なる。
  - 中断後のモデル状態が健全である保証を、バックエンドごとに検証する必要がある。

#### 案B: kill + 再spawn + 再ロードで代替(現行の延長)

進行中リクエストを放棄し、ワーカープロセスをkillして再spawn・再ロードする。
これは **現行の `JsonlTranscriber` がすでに行っている挙動**(4.3)である。

- **利点:** 実装が単純で、すでに動作している。バックエンドの中断APIに依存せず、
  どのモデルでも確実に処理を停止できる。中断後のモデル状態破損を考慮する必要がない。
- **欠点:** キャンセルのたびにモデル再ロードが発生し、その遅延(数秒〜数十秒、量子化と
  モデルサイズに依存)の間は次の認識を開始できない。頻繁なキャンセルでコストが累積する。

#### 推奨

**当面は案B(現行のkill+再spawn+再ロード)を正式な仕様として維持することを推奨する。**

理由:

- 案Bはすでに実装・動作しており、バックエンド(VibeVoice / faster-whisper)の中断対応の
  有無に依存せず確実に停止できる。ADR-0002 のbatch方式では1認識が比較的短時間で完了する
  想定であり、キャンセル頻度も限定的と考えられる。
- 案Aは「`generate` 中の協調的中断」というバックエンド横断で困難な前提に依存し、
  実装・検証コストが高い。プロトコル単純性(1リクエスト1レスポンス)も崩れる。

ただし、再ロードコストが体感品質を損なうことが計測で判明した場合(R-006のキャンセル応答性、
benchmarks 参照)は、案Aを `capabilities` で折衝可能な拡張として追加し、対応バックエンドのみ
有効化する段階導入を検討する。その際は 5.1 のハンドシェイクを前提とする。
