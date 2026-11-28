use super::*;
use crate::asr::{JsonlTranscriber, ModelResidency, Transcriber};
use std::{os::windows::io::AsRawHandle, time::Duration};
use tokio::{sync::watch, time::timeout};

const BOUND: Duration = Duration::from_secs(10);
const PEER: &str = r#"
import json, sys
if sys.argv[1] == "silent":
    sys.stdin.read()
    sys.exit(23)
print(json.dumps({"type":"ready","protocol":4,"version":"0.10.1",
    "model":{"task":"recognition","device":"CPU","weight_type":"Q8_0"}}), flush=True)
for line in sys.stdin:
    message = json.loads(line)
    if sys.argv[1] == "reject_upload" and message["type"] == "chunk":
        print(json.dumps({"type":"error","id":message["id"],
            "error":{"code":"invalid_chunk","padding":"x" * 65536}}), flush=True)
        # Keep both pipes open without draining the remaining upload.
        import threading
        threading.Event().wait()
    if message["type"] == "transcribe":
        print(json.dumps({"type":"end","id":message["id"],"stop":"complete","text":"restored"}), flush=True)
sys.exit(23)
"#;

fn fixture(root: &Path, mode: &str) -> WorkerCommand {
    let mut cmd =
        WorkerCommand::python(std::env::var_os("ASR_PYTHON").unwrap_or_else(|| "python".into()));
    cmd.args = vec!["-u".into(), "-c".into(), PEER.into(), mode.into()];
    const EXPECTED: &str = "1eec943f3fbf69947176e7c711415ad88a08184169f72cd31dca0ab071e14939";
    let artifacts = ["speech.exe", "speech.dll", "model.gguf"].map(|name| {
        let path = root.join(name);
        fs::write(&path, b"fake model").unwrap();
        (path, EXPECTED)
    });
    cmd.quantization = Some("8bit".into());
    cmd.native = Some(Speech {
        artifacts,
        device: "CPU",
        model_id: MODEL_ID,
        model_size: 10,
        japanese_only: true,
        accepts_prompt: false,
        bundled_runtime: true,
        cache_home: root.to_path_buf(),
        offline_only: true,
    });
    cmd
}

fn wav(root: &Path, pcm_bytes: u32) -> PathBuf {
    let path = root.join("audio.wav");
    let mut wav =
        b"RIFF\0\0\0\0WAVEfmt \x10\0\0\0\x01\0\x01\0\x80\x3e\0\0\0\x7d\0\0\x02\0\x10\0data\0\0\0\0"
            .to_vec();
    wav[4..8].copy_from_slice(&(36 + pcm_bytes).to_le_bytes());
    wav[40..44].copy_from_slice(&pcm_bytes.to_le_bytes());
    wav.resize(44 + pcm_bytes as usize, 0);
    fs::write(&path, wav).unwrap();
    path
}

#[tokio::test]
async fn silent_child_hits_ready_budget_before_install_deadline() {
    let root = tempfile::tempdir().unwrap();
    let mut client = JsonlTranscriber::new(
        fixture(root.path(), "silent"),
        BOUND,
        Duration::from_secs(6 * 60 * 60),
    );
    client.native_ready_budget = Duration::from_millis(1);
    let result = timeout(BOUND, client.load("8bit")).await.unwrap();
    assert!(matches!(result, Err(AsrError::Timeout)));
    assert!(!client.health().await.unwrap().loaded);
}

async fn exit_idle_peer(client: &JsonlTranscriber) {
    let mut state = client.state.lock().await;
    let worker = state.running.as_mut().unwrap();
    drop(worker.stdin.take());
    let exit = timeout(BOUND, worker.child.wait()).await.unwrap().unwrap();
    assert_eq!(exit.code(), Some(23));
}

#[tokio::test]
async fn idle_exit_invalidates_residency_before_load_health_and_transcribe() {
    let root = tempfile::tempdir().unwrap();
    let client = JsonlTranscriber::new(fixture(root.path(), "ready"), BOUND, BOUND);
    client.load("8bit").await.unwrap();
    let residency = client.model_residency().unwrap();
    assert_eq!(*residency.borrow(), ModelResidency::Resident(1));

    exit_idle_peer(&client).await;
    client.load("8bit").await.unwrap();
    assert_eq!(*residency.borrow(), ModelResidency::Resident(2));

    exit_idle_peer(&client).await;
    assert!(!client.health().await.unwrap().loaded);
    assert_eq!(*residency.borrow(), ModelResidency::Absent);
    client.load("8bit").await.unwrap();

    exit_idle_peer(&client).await;
    let (_sender, cancel) = watch::channel(false);
    let result = client
        .transcribe(&wav(root.path(), 2), None, cancel)
        .await
        .unwrap();
    assert_eq!(result.text, "restored");
    assert_eq!(*residency.borrow(), ModelResidency::Resident(4));
    client.shutdown().await.unwrap();
}

#[tokio::test]
async fn a_chunk_rejection_is_drained_while_upload_is_blocked() {
    let root = tempfile::tempdir().unwrap();
    let client = JsonlTranscriber::new(fixture(root.path(), "reject_upload"), BOUND, BOUND);
    client.load("8bit").await.unwrap();
    let (_sender, cancel) = watch::channel(false);
    let result = timeout(
        BOUND,
        client.transcribe(&wav(root.path(), 512 * 1024), None, cancel),
    )
    .await
    .unwrap();
    assert!(matches!(
        result,
        Err(AsrError::Worker(ref error)) if error.code == "speech_cpp_unavailable"
    ));
    assert!(!client.health().await.unwrap().loaded);
}

#[tokio::test]
async fn resumed_worker_is_owned_and_job_close_kills_it() {
    use crate::asr::worker_process_command;
    use windows::Win32::Foundation::{BOOL, HANDLE};
    use windows::Win32::System::JobObjects::IsProcessInJob;

    let root = tempfile::tempdir().unwrap();
    let cmd = fixture(root.path(), "ready");
    let (mut child, job) = spawn_owned(&mut worker_process_command(&cmd, false))
        .await
        .unwrap();
    let mut in_job = BOOL::default();
    // SAFETY: Both handles and the output parameter are live for this call.
    unsafe {
        IsProcessInJob(
            HANDLE(child.raw_handle().unwrap()),
            HANDLE(job.as_raw_handle()),
            &mut in_job,
        )
    }
    .unwrap();
    assert!(in_job.as_bool());
    let mut line = String::new();
    use tokio::io::AsyncBufReadExt;
    let mut stdout = tokio::io::BufReader::new(child.stdout.take().unwrap());
    timeout(BOUND, stdout.read_line(&mut line))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        serde_json::from_str::<Value>(&line).unwrap()["type"],
        "ready"
    );
    assert!(child.stdin.is_some());
    assert!(child.try_wait().unwrap().is_none());
    drop(job);
    // Keep stdin open so the peer can only leave its read loop when killed.
    let _stdin = child.stdin.take().unwrap();
    timeout(BOUND, child.wait()).await.unwrap().unwrap();
}
