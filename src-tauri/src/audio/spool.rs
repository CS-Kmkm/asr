//! A bounded, append-only recording checkpoint. Disk I/O belongs to the
//! checkpoint thread, never to the input callback.
use super::{wav, SharedCapture};
use crate::{
    state::PipelineMode,
    storage::{FailedTakeRetention, Storage},
    types::TRANSLATION_TARGET_LANGUAGES,
    TempArtifact,
};
use std::{
    fs::{self, File, OpenOptions},
    io::{self, Read, Seek, Write},
    path::{Path, PathBuf},
    sync::{mpsc, Arc, Mutex},
    thread,
    time::Duration,
};

const MAGIC: &[u8; 8] = b"LVIPCM05";
const TRANSLATE_MAGIC: &[u8; 8] = b"LVIPCM06";
const EDIT_MAGIC: &[u8; 8] = b"LVIPCM07";
const MAX_EDIT_SOURCE_BYTES: usize = 3 * 1_048_576;
const INTERVAL: Duration = Duration::from_secs(1);
const EXPIRY: Duration = Duration::from_secs(24 * 60 * 60);
const MAX_DATA: usize = 32 * 1024 * 1024;
const MAX_SAMPLES_PER_WRITE: usize = 384_000;

#[derive(Clone)]
pub(crate) struct Policy {
    storage: Storage,
    epoch: i64,
    context: CheckpointContext,
}

#[derive(Clone)]
enum CheckpointContext {
    Dictate,
    Translate([u8; 2]),
    Edit(String),
}

impl CheckpointContext {
    fn header_size(&self) -> usize {
        20 + match self {
            Self::Dictate => 0,
            Self::Translate(_) => 2,
            Self::Edit(source) => 4 + source.len(),
        }
    }

    fn write_header(&self, output: &mut File, rate: u32, epoch: i64) -> io::Result<()> {
        output.write_all(match self {
            Self::Dictate => MAGIC,
            Self::Translate(_) => TRANSLATE_MAGIC,
            Self::Edit(_) => EDIT_MAGIC,
        })?;
        output.write_all(&rate.to_le_bytes())?;
        output.write_all(&epoch.to_le_bytes())?;
        match self {
            Self::Dictate => Ok(()),
            Self::Translate(language) => output.write_all(language),
            Self::Edit(source) => {
                output.write_all(&(source.len() as u32).to_le_bytes())?;
                output.write_all(source.as_bytes())
            }
        }
    }

    fn read(magic: &[u8], input: &mut File) -> io::Result<Self> {
        match magic {
            bytes if bytes == MAGIC => Ok(Self::Dictate),
            bytes if bytes == TRANSLATE_MAGIC => {
                let mut bytes = [0; 2];
                input.read_exact(&mut bytes)?;
                let valid = std::str::from_utf8(&bytes)
                    .is_ok_and(|tag| TRANSLATION_TARGET_LANGUAGES.contains(&tag));
                if !valid {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "invalid checkpoint translation target",
                    ));
                }
                Ok(Self::Translate(bytes))
            }
            bytes if bytes == EDIT_MAGIC => {
                let mut len = [0; 4];
                input.read_exact(&mut len)?;
                let len = u32::from_le_bytes(len) as usize;
                if !(1..=MAX_EDIT_SOURCE_BYTES).contains(&len) {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "invalid checkpoint edit length",
                    ));
                }
                let mut bytes = vec![0; len];
                input.read_exact(&mut bytes)?;
                let text = String::from_utf8(bytes).map_err(|error| {
                    io::Error::new(io::ErrorKind::InvalidData, error.utf8_error())
                })?;
                if text.trim().is_empty() {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "empty checkpoint edit source",
                    ));
                }
                Ok(Self::Edit(text))
            }
            _ => Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "invalid checkpoint version",
            )),
        }
    }
}

impl std::fmt::Debug for Policy {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("CaptureSpoolPolicy")
    }
}

impl Policy {
    pub(crate) fn new(
        storage: Storage,
        mode: PipelineMode,
        target_language: Option<&str>,
        selected_text: Option<&str>,
    ) -> Option<Self> {
        let context = match mode {
            PipelineMode::Dictate => CheckpointContext::Dictate,
            PipelineMode::Translate => {
                let language =
                    target_language.filter(|tag| TRANSLATION_TARGET_LANGUAGES.contains(tag))?;
                CheckpointContext::Translate(language.as_bytes().try_into().ok()?)
            }
            PipelineMode::Edit => {
                let text = selected_text.filter(|text| {
                    text.len() <= MAX_EDIT_SOURCE_BYTES && !text.trim().is_empty()
                })?;
                CheckpointContext::Edit(text.to_owned())
            }
            PipelineMode::Ask | PipelineMode::SelectedText => return None,
        };
        let epoch = storage.capture_spool_epoch().ok()?;
        Some(Self {
            storage,
            epoch,
            context,
        })
    }

    fn is_current(&self) -> bool {
        let eligible = self
            .storage
            .get_settings_with_repairs()
            .is_ok_and(|(settings, repairs)| {
                !repairs.contains("historyRetention")
                    && !repairs.contains("deleteAudioAfterProcessing")
                    && FailedTakeRetention::for_settings(&settings)
                        == Some(FailedTakeRetention::Normal)
            });
        eligible && self.storage.capture_spool_epoch().ok() == Some(self.epoch)
    }
}

#[derive(Clone, Debug)]
pub struct Checkpoint {
    pub(crate) token: String,
    pub(crate) epoch: i64,
    pub(crate) path: PathBuf,
}

impl Checkpoint {
    pub(crate) fn unlink(&self) {
        let _ = fs::remove_file(&self.path);
    }
}

pub(super) struct CaptureSpool {
    pub(super) checkpoint: Checkpoint,
    pub(super) storage: Storage,
    stop: Option<mpsc::Sender<()>>,
    worker: Option<thread::JoinHandle<()>>,
    #[cfg(target_os = "windows")]
    capture: Arc<Mutex<Arc<Mutex<SharedCapture>>>>,
}

impl CaptureSpool {
    pub(super) fn start(
        policy: &Policy,
        rate: u32,
        capture: &Arc<Mutex<SharedCapture>>,
    ) -> Option<Self> {
        let mut random = [0u8; 16];
        getrandom::fill(&mut random).ok()?;
        let token: String = random.iter().map(|byte| format!("{byte:02x}")).collect();
        let epoch = policy.epoch;
        let path = policy.storage.capture_spool_dir().join(format!(
            "capture-{}-{epoch}-{token}.pcm",
            std::process::id(),
        ));
        let checkpoint = Checkpoint { token, epoch, path };
        let worker_checkpoint = checkpoint.clone();
        let (stop, receiver) = mpsc::channel();
        let capture = Arc::new(Mutex::new(Arc::clone(capture)));
        let worker_capture = Arc::clone(&capture);
        let policy = policy.clone();
        let storage = policy.storage.clone();
        let worker = thread::Builder::new()
            .name("capture-checkpoint".into())
            .spawn(move || {
                if write_checkpoint(&policy, &worker_checkpoint, rate, &worker_capture, receiver)
                    .is_err()
                {
                    crate::worker_log::record_startup_detail("Recording checkpoint unavailable.\n");
                }
                if policy
                    .storage
                    .capture_spool_pending(&worker_checkpoint.token, policy.epoch)
                    .is_ok_and(|pending| !pending)
                {
                    worker_checkpoint.unlink();
                }
            })
            .ok()?;
        Some(Self {
            checkpoint,
            storage,
            stop: Some(stop),
            worker: Some(worker),
            #[cfg(target_os = "windows")]
            capture,
        })
    }

    #[cfg(all(test, windows))]
    pub(super) fn fixture(storage: &Storage, capture: &Arc<Mutex<SharedCapture>>) -> Self {
        let epoch = storage.capture_spool_epoch().unwrap();
        let token = "0123456789abcdef0123456789abcdef".to_owned();
        let directory = storage.capture_spool_dir();
        fs::create_dir_all(&directory).unwrap();
        let path = directory.join(format!(
            "capture-{}-{epoch}-{token}.pcm",
            std::process::id()
        ));
        let mut file = File::create(&path).unwrap();
        CheckpointContext::Dictate
            .write_header(&mut file, 16_000, epoch)
            .unwrap();
        write_block(&mut file, &[1, 0, 2, 0]).unwrap();
        file.sync_data().unwrap();
        Self {
            checkpoint: Checkpoint { token, epoch, path },
            storage: storage.clone(),
            stop: None,
            worker: None,
            capture: Arc::new(Mutex::new(Arc::clone(capture))),
        }
    }

    #[cfg(target_os = "windows")]
    pub(super) fn use_capture(&self, capture: &Arc<Mutex<SharedCapture>>) {
        if let Ok(mut current) = self.capture.lock() {
            *current = Arc::clone(capture);
        }
    }

    pub(super) fn flush_and_stop(&mut self) {
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(());
        }
        self.join_writer();
    }

    fn join_writer(&mut self) {
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

impl Drop for CaptureSpool {
    fn drop(&mut self) {
        // Stop the writer. The pipeline decides when the file can be removed.
        self.stop.take();
        self.join_writer();
    }
}

fn write_checkpoint(
    policy: &Policy,
    checkpoint: &Checkpoint,
    rate: u32,
    capture: &Mutex<Arc<Mutex<SharedCapture>>>,
    stop: mpsc::Receiver<()>,
) -> io::Result<()> {
    if !policy.is_current()
        || !policy
            .storage
            .claim_capture_spool(policy.epoch)
            .map_err(io::Error::other)?
    {
        return Ok(());
    }
    fs::create_dir_all(policy.storage.capture_spool_dir())?;
    let mut output = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&checkpoint.path)?;
    let mut written = 0;
    let mut remaining = MAX_DATA - policy.context.header_size();
    let mut finishing = false;
    loop {
        if !finishing {
            match stop.recv_timeout(INTERVAL) {
                Ok(()) => finishing = true,
                Err(mpsc::RecvTimeoutError::Timeout) => {}
                Err(mpsc::RecvTimeoutError::Disconnected) => break,
            }
        }
        if !policy.is_current()
            || !policy
                .storage
                .capture_spool_pending(&checkpoint.token, policy.epoch)
                .map_err(io::Error::other)?
        {
            break;
        }
        if remaining < 10 {
            break;
        }
        let samples = {
            let capture = Arc::clone(
                &*capture
                    .lock()
                    .map_err(|_| io::Error::other("capture source lock unavailable"))?,
            );
            let capture = capture
                .lock()
                .map_err(|_| io::Error::other("capture lock unavailable"))?;
            let end = capture
                .samples
                .len()
                .min(written + MAX_SAMPLES_PER_WRITE)
                .min(written + (remaining - 8) / 2);
            capture
                .samples
                .get(written..end)
                .unwrap_or_default()
                .to_vec()
        };
        if samples.is_empty() {
            if finishing {
                break;
            }
            continue;
        }
        let pcm: Vec<u8> = samples
            .iter()
            .flat_map(|value| {
                let sample = if value.is_finite() {
                    value.clamp(-1.0, 1.0)
                } else {
                    0.0
                };
                ((sample * f32::from(i16::MAX)).round() as i16).to_le_bytes()
            })
            .collect();
        if written == 0 {
            policy
                .context
                .write_header(&mut output, rate, policy.epoch)?;
        }
        let last_complete = output.stream_position()?;
        if let Err(error) = write_block(&mut output, &pcm).and_then(|()| output.sync_data()) {
            let _ = output.set_len(last_complete);
            return Err(error);
        }
        remaining -= 8 + pcm.len();
        written += samples.len();
    }
    Ok(())
}

fn block_checksum(bytes: &[u8]) -> u32 {
    // FNV-1a detects corruption in the final block independently of its length.
    bytes.iter().fold(0x811c_9dc5u32, |hash, &byte| {
        (hash ^ u32::from(byte)).wrapping_mul(0x0100_0193)
    })
}

fn write_block(output: &mut impl Write, bytes: &[u8]) -> io::Result<()> {
    output.write_all(&(bytes.len() as u32).to_le_bytes())?;
    output.write_all(&block_checksum(bytes).to_le_bytes())?;
    output.write_all(bytes)
}

fn read_prefix(mut input: impl Read, mut remaining: usize) -> io::Result<Vec<u8>> {
    let mut bytes = Vec::new();
    while remaining >= 10 {
        let mut frame = [0; 8];
        if let Err(error) = input.read_exact(&mut frame) {
            if error.kind() == io::ErrorKind::UnexpectedEof {
                break;
            }
            return Err(error);
        }
        let len = u32::from_le_bytes(frame[..4].try_into().unwrap()) as usize;
        let checksum = u32::from_le_bytes(frame[4..].try_into().unwrap());
        if len == 0
            || !len.is_multiple_of(2)
            || len > (remaining - 8).min(MAX_SAMPLES_PER_WRITE * 2)
        {
            break;
        }
        let start = bytes.len();
        bytes.resize(start + len, 0);
        match input.read_exact(&mut bytes[start..]) {
            Ok(()) if block_checksum(&bytes[start..]) == checksum => remaining -= 8 + len,
            Ok(()) => {
                bytes.truncate(start);
                break;
            }
            Err(error) if error.kind() == io::ErrorKind::UnexpectedEof => {
                bytes.truncate(start);
                break;
            }
            Err(error) => return Err(error),
        }
    }
    Ok(bytes)
}

pub(crate) fn delete_stale(storage: &Storage, current_epoch: i64) {
    let Ok(entries) = fs::read_dir(storage.capture_spool_dir()) else {
        return;
    };
    for entry in entries.flatten() {
        if entry.file_type().is_ok_and(|kind| kind.is_file())
            && entry
                .file_name()
                .to_str()
                .and_then(checkpoint_name)
                .is_some_and(|name| {
                    parse_name(name).is_none_or(|(_, epoch, _)| epoch < current_epoch)
                })
        {
            let _ = fs::remove_file(entry.path());
        }
    }
}

pub(crate) fn recover(storage: &Storage, policy_readable: bool) {
    let Ok(entries) = fs::read_dir(storage.capture_spool_dir()) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(name) = name.to_str().and_then(checkpoint_name) else {
            continue;
        };
        if !entry.file_type().is_ok_and(|kind| kind.is_file()) {
            continue;
        }
        let path = entry.path();
        let Some((pid, epoch, token)) = parse_name(name) else {
            let _ = fs::remove_file(path);
            continue;
        };
        if storage
            .capture_spool_pending(token, epoch)
            .is_ok_and(|pending| !pending)
        {
            let _ = fs::remove_file(path);
            continue;
        }
        let metadata = entry.metadata();
        #[cfg(windows)]
        let created = {
            use std::os::windows::fs::MetadataExt;
            metadata.as_ref().ok().map(MetadataExt::creation_time)
        };
        #[cfg(not(windows))]
        let created = None;
        if checkpoint_owner_is_live(pid, created) {
            continue;
        }
        let expired = metadata
            .and_then(|info| info.modified())
            .ok()
            .and_then(|time| time.elapsed().ok())
            .is_some_and(|age| age > EXPIRY);
        if expired {
            let _ = fs::remove_file(&path);
        } else if policy_readable {
            let Ok((settings, repairs)) = storage.get_settings_with_repairs() else {
                continue;
            };
            if repairs.contains("historyRetention")
                || repairs.contains("deleteAudioAfterProcessing")
            {
                continue;
            }
            let allowed =
                FailedTakeRetention::for_settings(&settings) == Some(FailedTakeRetention::Normal);
            if !allowed || recover_file(storage, &path, token, &settings.asr_backend).is_ok() {
                let _ = fs::remove_file(&path);
            }
        }
    }
}

fn checkpoint_owner_is_live(pid: u32, _created: Option<u64>) -> bool {
    if !crate::process_is_live(pid) {
        return false;
    }
    #[cfg(windows)]
    if let (Some(checkpoint), Some(process)) = (_created, process_creation_time(pid)) {
        // A process launched after this file was created inherited a reused PID.
        return process <= checkpoint;
    }
    true
}

#[cfg(windows)]
fn process_creation_time(pid: u32) -> Option<u64> {
    use windows::Win32::{
        Foundation::{CloseHandle, FILETIME},
        System::Threading::{GetProcessTimes, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION},
    };

    let process = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) }.ok()?;
    let mut created = FILETIME::default();
    let mut exit = FILETIME::default();
    let mut kernel = FILETIME::default();
    let mut user = FILETIME::default();
    // SAFETY: The process handle and output slots are valid for the duration of the call.
    let result =
        unsafe { GetProcessTimes(process, &mut created, &mut exit, &mut kernel, &mut user) };
    let _ = unsafe { CloseHandle(process) };
    result.ok()?;
    Some((u64::from(created.dwHighDateTime) << 32) | u64::from(created.dwLowDateTime))
}

fn checkpoint_name(name: &str) -> Option<&str> {
    name.strip_prefix("capture-")?.strip_suffix(".pcm")?;
    Some(name)
}

fn parse_name(name: &str) -> Option<(u32, i64, &str)> {
    let (pid, rest) = name
        .strip_prefix("capture-")?
        .strip_suffix(".pcm")?
        .split_once('-')?;
    let (epoch, token) = rest.split_once('-')?;
    if token.len() != 32 || !token.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return None;
    }
    Some((pid.parse().ok()?, epoch.parse().ok()?, token))
}

fn recover_file(
    storage: &Storage,
    source: &Path,
    token: &str,
    backend: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    let mut input = File::open(source)?;
    let read_header = (|| -> io::Result<_> {
        let mut header = [0u8; 20];
        input.read_exact(&mut header)?;
        let rate = u32::from_le_bytes(header[8..12].try_into().unwrap());
        let epoch = i64::from_le_bytes(header[12..].try_into().unwrap());
        if !(8_000..=384_000).contains(&rate) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "invalid checkpoint header",
            ));
        }
        let context = CheckpointContext::read(&header[..8], &mut input)?;
        Ok((rate, epoch, context))
    })();
    let (rate, epoch, context) = match read_header {
        Ok(header) => header,
        Err(error)
            if matches!(
                error.kind(),
                io::ErrorKind::UnexpectedEof | io::ErrorKind::InvalidData
            ) =>
        {
            return Ok(())
        }
        Err(error) => return Err(error.into()),
    };
    if !storage.capture_spool_pending(token, epoch)? {
        return Ok(());
    }
    let bytes = read_prefix(input, MAX_DATA - context.header_size())?;
    if bytes.iter().all(|byte| *byte == 0) {
        return Ok(());
    }
    let path: PathBuf = wav::temporary_wav_path(None);
    let target = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&path)?;
    let _cleanup = TempArtifact::new(path.clone(), false);
    wav::write_pcm16_bytes(target, &bytes, rate)?;
    let (mode, edit_source, translation_target) = match &context {
        CheckpointContext::Dictate => (PipelineMode::Dictate, None, None),
        CheckpointContext::Translate(language) => (
            PipelineMode::Translate,
            None,
            Some(std::str::from_utf8(language)?),
        ),
        CheckpointContext::Edit(source) => (PipelineMode::Edit, Some(source.as_str()), None),
    };
    let item = crate::commands::failed_take_history_item(
        mode,
        backend,
        edit_source,
        translation_target,
        None,
        bytes.len() as u64 * 500 / u64::from(rate),
    )
    .expect("checkpoint modes have a retry route");
    storage.import_capture_spool(token, epoch, &item, &path)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{HistoryFilter, Settings};

    #[cfg(windows)]
    #[test]
    fn a_reused_pid_does_not_own_an_older_checkpoint() {
        let pid = std::process::id();
        let started = process_creation_time(pid).unwrap();
        assert!(!checkpoint_owner_is_live(pid, Some(started - 1)));
        assert!(checkpoint_owner_is_live(pid, Some(started + 1)));
    }

    #[test]
    fn checkpoint_only_recovers_the_prefix_before_a_torn_or_corrupt_block() {
        let mut frames = Vec::new();
        let good = [1, 0, 2, 0];
        write_block(&mut frames, &good).unwrap();
        let start = frames.len();
        write_block(&mut frames, &[3, 0, 4, 0]).unwrap();
        assert_eq!(
            read_prefix(&frames[..frames.len() - 2], MAX_DATA).unwrap(),
            good
        );
        frames[start + 8] ^= 0x80;
        write_block(&mut frames, &[5, 0]).unwrap();
        assert_eq!(read_prefix(frames.as_slice(), MAX_DATA).unwrap(), good);
    }

    #[test]
    fn normal_stop_flushes_samples_before_the_first_periodic_checkpoint() {
        let workspace = tempfile::tempdir().unwrap();
        let storage = Storage::open(&workspace.path().join("history.sqlite")).unwrap();
        storage
            .update_settings(&Settings {
                delete_audio_after_processing: false,
                ..Settings::default()
            })
            .unwrap();
        let policy = Policy::new(storage.clone(), PipelineMode::Dictate, None, None).unwrap();
        let checkpoint = Checkpoint {
            token: "0123456789abcdef0123456789abcdef".into(),
            epoch: policy.epoch,
            path: storage.capture_spool_dir().join("test.pcm"),
        };
        let capture = Mutex::new(Arc::new(Mutex::new(SharedCapture {
            samples: vec![1.0, -1.0],
            ..SharedCapture::default()
        })));
        let (stop, receiver) = mpsc::channel();
        stop.send(()).unwrap();
        drop(stop);
        write_checkpoint(&policy, &checkpoint, 16_000, &capture, receiver).unwrap();

        let contents = fs::read(checkpoint.path).unwrap();
        assert_eq!(
            read_prefix(&contents[policy.context.header_size()..], MAX_DATA).unwrap(),
            [255, 127, 1, 128]
        );
    }

    #[test]
    fn committed_history_consumes_the_checkpoint_even_if_the_file_remains() {
        let workspace = tempfile::tempdir().unwrap();
        let storage = Storage::open(&workspace.path().join("history.sqlite")).unwrap();
        storage
            .update_settings(&Settings {
                delete_audio_after_processing: false,
                ..Settings::default()
            })
            .unwrap();
        let epoch = storage.capture_spool_epoch().unwrap();
        let token = "0123456789abcdef0123456789abcdef";
        let directory = storage.capture_spool_dir();
        fs::create_dir_all(&directory).unwrap();
        let path = directory.join(format!("capture-0-{epoch}-{token}.pcm"));
        let mut file = File::create(&path).unwrap();
        CheckpointContext::Dictate
            .write_header(&mut file, 16_000, epoch)
            .unwrap();
        write_block(&mut file, &[1, 0, 2, 0]).unwrap();
        drop(file);
        let checkpoint = Checkpoint {
            token: token.into(),
            epoch,
            path: path.clone(),
        };
        let scoped = storage.for_recorded_history(Some(checkpoint));
        let item = crate::commands::failed_take_history_item(
            PipelineMode::Dictate,
            "mock",
            None,
            None,
            None,
            1,
        )
        .unwrap();
        let wav = workspace.path().join("take.wav");
        wav::write_pcm16_bytes(File::create(&wav).unwrap(), &[1, 0, 2, 0], 16_000).unwrap();
        let bytes = fs::read(&path).unwrap();
        assert!(storage.capture_spool_pending(token, epoch).unwrap());
        assert!(scoped
            .add_history_with_audio(&item, Some(&wav.with_extension("missing")))
            .is_err());
        assert!(storage.capture_spool_pending(token, epoch).unwrap());
        assert_eq!(fs::read(&path).unwrap(), bytes);
        assert!(storage
            .list_history(HistoryFilter::All, 10)
            .unwrap()
            .is_empty());
        assert!(scoped.add_history_with_audio(&item, Some(&wav)).unwrap());
        assert!(!path.exists());
        // Model a failed unlink by presenting the identical checkpoint at restart.
        fs::write(&path, bytes).unwrap();
        recover(&storage, true);
        assert!(!path.exists());
        assert_eq!(
            storage.list_history(HistoryFilter::All, 10).unwrap().len(),
            1
        );
    }

    #[test]
    fn privacy_actions_delete_older_epochs_and_startup_retries_stale_cleanup() {
        let workspace = tempfile::tempdir().unwrap();
        let storage = Storage::open(&workspace.path().join("history.sqlite")).unwrap();
        let kept = Settings {
            delete_audio_after_processing: false,
            ..Settings::default()
        };
        storage.update_settings(&kept).unwrap();
        let epoch = storage.capture_spool_epoch().unwrap();
        let directory = storage.capture_spool_dir();
        fs::create_dir_all(&directory).unwrap();
        let token = "0123456789abcdef0123456789abcdef";
        // Use a live PID. Privacy invalidation takes precedence over process liveness.
        let pid = std::process::id();
        let stale = directory.join(format!("capture-{pid}-{epoch}-{token}.pcm"));
        let next = directory.join(format!("capture-{pid}-{}-{token}.pcm", epoch + 1));
        fs::write(&stale, b"stale").unwrap();
        fs::write(&next, b"next").unwrap();
        storage.delete_all_history().unwrap();
        assert!(!stale.exists());
        assert!(next.exists());
        fs::write(&stale, b"stale").unwrap();
        recover(&storage, false);
        assert!(!stale.exists());
        assert!(next.exists());
        storage
            .update_settings_and_apply_history_policy(&Settings {
                delete_audio_after_processing: true,
                ..kept
            })
            .unwrap();
        assert!(!next.exists());
    }

    #[cfg(windows)]
    #[test]
    fn killed_process_checkpoint_is_recovered_on_startup() {
        use std::process::{Child, Command, Stdio};
        use std::time::Instant;

        struct ChildGuard(Child);
        impl Drop for ChildGuard {
            fn drop(&mut self) {
                let _ = self.0.kill();
                let _ = self.0.wait();
            }
        }
        let workspace = tempfile::tempdir().unwrap();
        let database = workspace.path().join("test.sqlite3");
        let storage = Storage::open(&database).unwrap();
        storage
            .update_settings(&Settings {
                delete_audio_after_processing: false,
                ..Settings::default()
            })
            .unwrap();
        let mut child = ChildGuard(
            Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "audio::spool::tests::write_spool_until_terminated",
                    "--ignored",
                ])
                .env("ASR_TEST_CAPTURE_SPOOL_DB", &database)
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
                .unwrap(),
        );
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let written = fs::read_dir(storage.capture_spool_dir())
                .ok()
                .is_some_and(|entries| {
                    entries
                        .flatten()
                        .any(|entry| entry.metadata().is_ok_and(|info| info.len() == 3228))
                });
            if written {
                break;
            }
            assert!(
                Instant::now() < deadline && child.0.try_wait().unwrap().is_none(),
                "the child did not write its checkpoint"
            );
            thread::sleep(Duration::from_millis(50));
        }
        child.0.kill().unwrap();
        child.0.wait().unwrap();
        recover(&storage, true);
        let history = storage.list_history(HistoryFilter::All, 10).unwrap();
        assert_eq!(history.len(), 1);
        assert_eq!(history[0].duration_ms, Some(100));
        assert_eq!(
            storage.history_audio(history[0].id).unwrap().unwrap().len(),
            3244
        );
        assert!(fs::read_dir(storage.capture_spool_dir())
            .unwrap()
            .next()
            .is_none());
    }

    #[cfg(windows)]
    #[test]
    #[ignore = "spawned only by the controlled process-death fixture"]
    fn write_spool_until_terminated() {
        let database = PathBuf::from(std::env::var_os("ASR_TEST_CAPTURE_SPOOL_DB").unwrap());
        let storage = Storage::open(&database).unwrap();
        let policy = Policy::new(storage, PipelineMode::Dictate, None, None).unwrap();
        let capture = Arc::new(Mutex::new(SharedCapture {
            samples: vec![0.25; 1600],
            recording: true,
            ..SharedCapture::default()
        }));
        let _spool = CaptureSpool::start(&policy, 16_000, &capture).unwrap();
        // The parent normally kills us first. The timeout prevents an orphan
        // test process if the parent itself is interrupted.
        thread::sleep(Duration::from_secs(15));
    }

    #[test]
    fn recovered_translation_uses_the_checkpoint_language_and_audio() {
        let workspace = tempfile::tempdir().unwrap();
        let storage = Storage::open(&workspace.path().join("test.sqlite3")).unwrap();
        storage
            .update_settings(&Settings {
                delete_audio_after_processing: false,
                translation_target_language: "en".into(),
                ..Settings::default()
            })
            .unwrap();
        let spool = workspace.path().join("translation.pcm");
        let mut file = File::create(&spool).unwrap();
        file.write_all(TRANSLATE_MAGIC).unwrap();
        file.write_all(&16_000u32.to_le_bytes()).unwrap();
        file.write_all(&storage.capture_spool_epoch().unwrap().to_le_bytes())
            .unwrap();
        file.write_all(b"ja").unwrap();
        write_block(&mut file, &[1, 0, 2, 0]).unwrap();
        drop(file);

        recover_file(&storage, &spool, "translation-token", "mock").unwrap();
        let rows = storage.list_history(HistoryFilter::All, 10).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].mode, "translate");
        assert_eq!(rows[0].target_language.as_deref(), Some("ja"));
        let audio = storage.history_audio(rows[0].id).unwrap().unwrap();
        assert_eq!(&audio[44..], &[1, 0, 2, 0]);
    }

    #[test]
    fn edit_checkpoint_restores_only_the_saved_selection_and_audio() {
        let workspace = tempfile::tempdir().unwrap();
        let storage = Storage::open(&workspace.path().join("test.sqlite3")).unwrap();
        storage
            .update_settings(&Settings {
                delete_audio_after_processing: false,
                ..Settings::default()
            })
            .unwrap();
        let path = workspace.path().join("edit.pcm");
        let policy = Policy::new(
            storage.clone(),
            PipelineMode::Edit,
            None,
            Some("選択した文"),
        )
        .unwrap();
        let mut file = File::create(&path).unwrap();
        policy
            .context
            .write_header(&mut file, 16_000, policy.epoch)
            .unwrap();
        write_block(&mut file, &[1, 0, 2, 0]).unwrap();
        drop(file);

        recover_file(&storage, &path, "edit-token", "mock").unwrap();
        let rows = storage.list_history(HistoryFilter::All, 10).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].mode, "edit");
        assert_eq!(rows[0].source_text.as_deref(), Some("選択した文"));
        assert_eq!(
            &storage.history_audio(rows[0].id).unwrap().unwrap()[44..],
            &[1, 0, 2, 0]
        );
        storage.delete_all_history().unwrap();
        recover_file(&storage, &path, "unused-edit-token", "mock").unwrap();
        assert!(storage
            .list_history(HistoryFilter::All, 10)
            .unwrap()
            .is_empty());
    }

    #[test]
    fn repeated_import_cannot_recreate_a_deleted_recovered_take() {
        let workspace = tempfile::tempdir().unwrap();
        let storage = Storage::open(&workspace.path().join("test.sqlite3")).unwrap();
        storage
            .update_settings(&Settings {
                delete_audio_after_processing: false,
                ..Settings::default()
            })
            .unwrap();
        let spool = workspace.path().join("take.pcm");
        let mut file = File::create(&spool).unwrap();
        file.write_all(MAGIC).unwrap();
        file.write_all(&16_000u32.to_le_bytes()).unwrap();
        file.write_all(&storage.capture_spool_epoch().unwrap().to_le_bytes())
            .unwrap();
        write_block(&mut file, &[1, 0]).unwrap();
        // The first complete frame survives a torn trailing frame header.
        file.write_all(&[0x55]).unwrap();
        drop(file);

        recover_file(&storage, &spool, "sample-token", "mock").unwrap();
        recover_file(&storage, &spool, "sample-token", "mock").unwrap();
        let history = storage.list_history(HistoryFilter::All, 10).unwrap();
        assert_eq!(history.len(), 1);
        let audio = storage.history_audio(history[0].id).unwrap().unwrap();
        assert_eq!(&audio[44..], &[1, 0]);
        storage.delete_all_history().unwrap();
        // A separate, never-imported checkpoint from before the deletion
        // must also be ineligible.
        recover_file(&storage, &spool, "another-token", "mock").unwrap();
        assert!(storage
            .list_history(HistoryFilter::All, 10)
            .unwrap()
            .is_empty());
    }
}
