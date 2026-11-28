use super::{worker_error, AsrError, Speech};
use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::{
    collections::HashMap,
    fs::{self, File},
    io::{self, Read},
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, LazyLock, Mutex,
    },
};
use tokio::{sync::Mutex as AsyncMutex, time::Instant};

const MAX_RUNTIME_FILE_BYTES: u64 = 128 * 1024 * 1024;
const HASH_BLOCK_BYTES: usize = 1024 * 1024;
type PathVerification = Arc<AsyncMutex<Option<Verified>>>;
static VERIFIED: LazyLock<Mutex<HashMap<PathBuf, PathVerification>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
struct Stamp {
    size: u64,
    modified: u64,
    volume: Option<u64>,
    file_id: Option<u64>,
}

#[derive(Deserialize)]
pub(in crate::asr) struct Receipt {
    path: PathBuf,
    sha256: String,
    stamp: Stamp,
}

struct Verified {
    stamp: Stamp,
    sha256: String,
}

struct CancelOnDrop(Arc<AtomicBool>);

impl Drop for CancelOnDrop {
    fn drop(&mut self) {
        self.0.store(true, Ordering::Relaxed);
    }
}

impl Speech {
    /// Only absent files are installable. A present invalid file is preserved.
    pub(in crate::asr) async fn verify_artifacts(
        &self,
        deadline: Instant,
        receipts: Arc<[Receipt]>,
    ) -> Result<bool, AsrError> {
        let cancellation = CancelOnDrop(Arc::new(AtomicBool::new(false)));
        let mut complete = true;
        for (index, (path, expected)) in self.artifacts.iter().enumerate() {
            let path = path.clone();
            let canonical = blocking(move || Ok(fs::canonicalize(path))).await?;
            let canonical = match canonical {
                Ok(path) => path,
                Err(error) if error.kind() == io::ErrorKind::NotFound => {
                    if index != 2 && self.bundled_runtime {
                        return Err(invalid("The installed speech.cpp runtime is incomplete"));
                    }
                    complete = false;
                    continue;
                }
                Err(error) => return Err(error.into()),
            };
            let slot = verification_slot(&canonical);
            // The blocking task owns the guard until it stops reading, even
            // when the caller has dropped its join handle.
            let mut entry = slot.lock_owned().await;
            let cancelled = cancellation.0.clone();
            let receipts = receipts.clone();
            let expected = *expected;
            let model_size = (index == 2).then_some(self.model_size);
            blocking(move || {
                let check = || check_active(&cancelled, deadline);
                check()?;
                let mut file = File::open(&canonical)?;
                let before = stamp(&file)?;
                if model_size.map_or(before.size > MAX_RUNTIME_FILE_BYTES, |size| {
                    before.size != size
                }) {
                    return Err(invalid("Native speech.cpp artifact has an unexpected size"));
                }
                verify_file(
                    &canonical, &mut file, before, expected, &receipts, &mut entry, check,
                )
            })
            .await?;
        }
        Ok(complete)
    }
}

fn verification_slot(path: &Path) -> PathVerification {
    VERIFIED
        .lock()
        .expect("artifact verification map")
        .entry(path.to_owned())
        .or_default()
        .clone()
}

async fn blocking<T: Send + 'static>(
    work: impl FnOnce() -> Result<T, AsrError> + Send + 'static,
) -> Result<T, AsrError> {
    tokio::task::spawn_blocking(work)
        .await
        .map_err(|_| AsrError::Crashed)?
}

fn verify_file(
    path: &Path,
    file: &mut File,
    before: Stamp,
    expected: &str,
    receipts: &[Receipt],
    entry: &mut Option<Verified>,
    check: impl Fn() -> Result<(), AsrError>,
) -> Result<(), AsrError> {
    if let Some(verified) = entry.as_ref().filter(|verified| verified.stamp == before) {
        return require_digest(&verified.sha256, expected);
    }
    *entry = None;
    let installed = before.file_id.is_some()
        && receipts.iter().any(|receipt| {
            receipt.sha256 == expected
                && receipt.stamp == before
                && fs::canonicalize(&receipt.path).is_ok_and(|reported| reported == path)
        });
    let sha256 = if installed {
        expected.to_owned()
    } else {
        hash_blocks(&mut *file, &check)?
    };
    check()?;
    if stamp(file)? != before || stamp(&File::open(path)?)? != before {
        return Err(invalid(
            "Native speech.cpp artifact changed during verification",
        ));
    }
    require_digest(&sha256, expected)?;
    *entry = Some(Verified {
        stamp: before,
        sha256,
    });
    Ok(())
}

fn check_active(cancelled: &AtomicBool, deadline: Instant) -> Result<(), AsrError> {
    if cancelled.load(Ordering::Relaxed) {
        return Err(AsrError::Cancelled);
    }
    if Instant::now() >= deadline {
        return Err(AsrError::Timeout);
    }
    Ok(())
}

fn hash_blocks(
    mut file: impl Read,
    check: impl Fn() -> Result<(), AsrError>,
) -> Result<String, AsrError> {
    let mut hash = Sha256::new();
    let mut block = vec![0; HASH_BLOCK_BYTES];
    loop {
        check()?;
        let count = file.read(&mut block)?;
        if count == 0 {
            return Ok(format!("{:x}", hash.finalize()));
        }
        hash.update(&block[..count]);
    }
}

fn require_digest(actual: &str, expected: &str) -> Result<(), AsrError> {
    if actual != expected {
        return Err(invalid(
            "Native speech.cpp artifact failed SHA-256 verification",
        ));
    }
    Ok(())
}

fn invalid(message: &str) -> AsrError {
    worker_error("speech_cpp_artifact_invalid", message)
}

#[cfg(windows)]
fn stamp(file: &File) -> io::Result<Stamp> {
    use std::os::windows::{fs::MetadataExt, io::AsRawHandle};
    use windows::Win32::Foundation::{FILETIME, HANDLE};

    #[repr(C)]
    #[derive(Default)]
    struct FileInformation {
        attributes: u32,
        created: FILETIME,
        accessed: FILETIME,
        written: FILETIME,
        volume: u32,
        size_high: u32,
        size_low: u32,
        links: u32,
        id_high: u32,
        id_low: u32,
    }
    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn GetFileInformationByHandle(handle: HANDLE, info: *mut FileInformation) -> i32;
    }
    let join = |high, low| (u64::from(high) << 32) | u64::from(low);
    let mut info = FileInformation::default();
    // SAFETY: The file handle is live and info matches BY_HANDLE_FILE_INFORMATION.
    if unsafe { GetFileInformationByHandle(HANDLE(file.as_raw_handle()), &mut info) } != 0 {
        return Ok(Stamp {
            size: join(info.size_high, info.size_low),
            modified: join(info.written.dwHighDateTime, info.written.dwLowDateTime),
            volume: Some(info.volume.into()),
            file_id: Some(join(info.id_high, info.id_low)),
        });
    }
    let metadata = file.metadata()?;
    Ok(Stamp {
        size: metadata.len(),
        modified: metadata.last_write_time(),
        volume: None,
        file_id: None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{io::Seek, time::Duration};

    #[test]
    fn hash_stops_between_blocks_after_cancel_or_deadline() {
        struct FirstBlock<F>(Option<F>);
        impl<F: FnOnce()> Read for FirstBlock<F> {
            fn read(&mut self, block: &mut [u8]) -> io::Result<usize> {
                self.0.take().expect("read continued after cancellation")();
                block.fill(1);
                Ok(block.len())
            }
        }

        let cancelled = Arc::new(AtomicBool::new(false));
        let deadline = Instant::now() + Duration::from_secs(10);
        let guard = CancelOnDrop(cancelled.clone());
        let reader = FirstBlock(Some(|| drop(guard)));
        assert!(matches!(
            hash_blocks(reader, || check_active(&cancelled, deadline)),
            Err(AsrError::Cancelled)
        ));
        let reader = FirstBlock::<fn()>(None);
        assert!(matches!(
            hash_blocks(reader, || check_active(
                &AtomicBool::new(false),
                Instant::now()
            )),
            Err(AsrError::Timeout)
        ));
    }

    #[tokio::test]
    async fn concurrent_verifications_share_the_result_and_changed_stamps_rehash() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("model.gguf");
        fs::write(&path, b"valid").unwrap();
        let expected = format!("{:x}", Sha256::digest(b"valid"));
        let verify = || async {
            let slot = verification_slot(&path);
            let mut entry = slot.lock_owned().await;
            let mut file = File::open(&path).unwrap();
            let before = stamp(&file).unwrap();
            let result = verify_file(&path, &mut file, before, &expected, &[], &mut entry, || {
                Ok(())
            });
            (result, file.stream_position().unwrap())
        };
        let (one, two) = tokio::join!(verify(), verify());
        assert!(one.0.is_ok() && two.0.is_ok());
        assert_eq!(one.1 + two.1, 5, "only one verifier read the file");

        fs::write(&path, b"corrupt").unwrap();
        let (result, bytes_read) = verify().await;
        assert!(result.is_err());
        assert_eq!(bytes_read, 7);
    }

    #[cfg(windows)]
    #[test]
    fn installer_receipt_skips_reads_only_for_matching_file_identity() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("model.gguf");
        fs::write(&path, b"valid").unwrap();
        let path = fs::canonicalize(path).unwrap();
        let mut file = File::open(&path).unwrap();
        let before = stamp(&file).unwrap();
        let expected = format!("{:x}", Sha256::digest(b"valid"));
        let receipts = [Receipt {
            path: path.clone(),
            sha256: expected.clone(),
            stamp: before.clone(),
        }];
        let mut entry = None;
        verify_file(
            &path,
            &mut file,
            before,
            &expected,
            &receipts,
            &mut entry,
            || Ok(()),
        )
        .unwrap();
        assert_eq!(file.stream_position().unwrap(), 0);
        let modified = file.metadata().unwrap().modified().unwrap();
        drop(file);
        let replacement = dir.path().join("replacement.gguf");
        fs::write(&replacement, b"wrong").unwrap();
        let file = File::options().write(true).open(&replacement).unwrap();
        file.set_times(fs::FileTimes::new().set_modified(modified))
            .unwrap();
        drop(file);
        fs::rename(replacement, &path).unwrap();
        let mut file = File::open(&path).unwrap();
        let before = stamp(&file).unwrap();
        assert_eq!(before.modified, receipts[0].stamp.modified);
        assert_ne!(before.file_id, receipts[0].stamp.file_id);
        let result = verify_file(
            &path,
            &mut file,
            before,
            &expected,
            &receipts,
            &mut entry,
            || Ok(()),
        );
        assert!(result.is_err());
        assert_eq!(file.stream_position().unwrap(), 5);
    }
}

#[cfg(not(windows))]
fn stamp(file: &File) -> io::Result<Stamp> {
    let metadata = file.metadata()?;
    let modified = metadata
        .modified()?
        .duration_since(std::time::UNIX_EPOCH)
        .map_err(io::Error::other)?;
    Ok(Stamp {
        size: metadata.len(),
        modified: modified.as_nanos() as u64,
        volume: None,
        file_id: None,
    })
}
