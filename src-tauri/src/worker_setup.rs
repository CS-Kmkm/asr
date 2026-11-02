//! Provision bundled worker dependencies into user-writable storage with uv.
use crate::{
    asr::{AsrError, WorkerError},
    environment,
};
use std::{
    path::{Path, PathBuf},
    process::Stdio,
    sync::OnceLock,
};
use tokio::process::Command;

static BUNDLED: OnceLock<Provision> = OnceLock::new();

/// Written into the environment after a successful base sync. It names the
/// inputs that sync aligned the environment with, so a later spawn can skip
/// the sync while they are unchanged.
const SYNC_MARKER: &str = ".asr-sync-fingerprint";
/// Part of every fingerprint; changing the sync command must change it too.
const SYNC_COMMAND_IDENTITY: &str = "uv sync --frozen --no-dev --inexact --python 3.12";

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Provision {
    pub(crate) source: PathBuf,
    pub(crate) environment: PathBuf,
    pub(crate) vibevoice: bool,
    pub(crate) uv: PathBuf,
}

impl Provision {
    pub(crate) fn python(&self) -> PathBuf {
        self.environment.join(if cfg!(windows) {
            "Scripts/python.exe"
        } else {
            "bin/python"
        })
    }

    fn command(&self) -> Command {
        let mut command = Command::new(&self.uv);
        command
            .env_clear()
            .envs(environment::child_environment())
            .env("UV_PROJECT_ENVIRONMENT", &self.environment)
            .args([
                "sync",
                "--frozen",
                "--no-dev",
                "--inexact",
                "--python",
                "3.12",
                "--project",
            ])
            .arg(&self.source)
            .current_dir(&self.source)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .kill_on_drop(true);
        if self.vibevoice {
            command.args(["--extra", "vibevoice"]);
        }
        #[cfg(windows)]
        command.creation_flags(0x0800_0000);
        command
    }

    fn marker(&self) -> PathBuf {
        self.environment.join(SYNC_MARKER)
    }

    /// A digest of everything a base sync aligns the environment with: the
    /// sync command, the app version, the requested extras, the interpreter
    /// path, the dependency lock and project files, and the environment's own
    /// `pyvenv.cfg` (rewritten whenever uv recreates the environment). The
    /// worker source is not part of it: it runs from the bundled resources,
    /// is not installed into the environment, and changes only with the app
    /// version. `None` when a required input cannot be read.
    fn fingerprint(&self) -> Option<String> {
        let mut digest = Fnv1a::default();
        digest.field(SYNC_COMMAND_IDENTITY.as_bytes());
        digest.field(env!("CARGO_PKG_VERSION").as_bytes());
        digest.field(if self.vibevoice { b"vibevoice" } else { b"" });
        digest.field(self.python().as_os_str().as_encoded_bytes());
        for name in ["uv.lock", "pyproject.toml"] {
            digest.field(&std::fs::read(self.source.join(name)).ok()?);
        }
        digest.field(&std::fs::read(self.source.join(".python-version")).unwrap_or_default());
        digest.field(&std::fs::read(self.environment.join("pyvenv.cfg")).ok()?);
        Some(format!("{:016x}", digest.finish()))
    }

    /// Whether the environment's interpreter exists and the environment still
    /// matches the inputs of its last successful base sync.
    pub(crate) fn is_synced(&self) -> bool {
        self.python().is_file()
            && self.fingerprint().is_some_and(|fingerprint| {
                std::fs::read_to_string(self.marker())
                    .is_ok_and(|recorded| recorded.trim() == fingerprint)
            })
    }

    /// `is_synced` without blocking the async runtime on file reads.
    pub(crate) async fn is_synced_async(&self) -> bool {
        let provision = self.clone();
        tokio::task::spawn_blocking(move || provision.is_synced())
            .await
            .unwrap_or(false)
    }

    /// Forget the last successful sync, so the next spawn syncs again.
    pub(crate) fn forget_sync(&self) {
        let _ = std::fs::remove_file(self.marker());
    }

    /// Record a successful sync. A marker that cannot be written only costs
    /// the next spawn a sync.
    pub(crate) fn record_sync(&self) {
        let Some(fingerprint) = self.fingerprint() else {
            return;
        };
        let partial = self.environment.join(format!("{SYNC_MARKER}.partial"));
        if std::fs::write(&partial, fingerprint).is_err()
            || std::fs::rename(&partial, self.marker()).is_err()
        {
            let _ = std::fs::remove_file(&partial);
        }
    }

    pub(crate) async fn prepare(&self) -> Result<(), AsrError> {
        // An interrupted or failed sync may leave the environment between
        // states, so it never keeps an earlier sync's marker.
        self.forget_sync();
        // Align base requirements while retaining optional packages the user
        // explicitly prepared for another feature.
        let status = self
            .command()
            .status()
            .await
            .map_err(|_| setup_error("worker_setup_unavailable"))?;
        if !status.success() || !self.python().is_file() {
            return Err(setup_error("worker_setup_failed"));
        }
        let provision = self.clone();
        let _ = tokio::task::spawn_blocking(move || provision.record_sync()).await;
        Ok(())
    }

    /// Optional packages are installed only by an explicit preparation action.
    /// Inexact sync preserves extras that a different backend still needs.
    pub(crate) async fn prepare_decision(
        &self,
        mut cancel: tokio::sync::watch::Receiver<bool>,
    ) -> Result<(), AsrError> {
        if *cancel.borrow() {
            return Err(AsrError::Cancelled);
        }
        let mut command = self.command();
        command.args(["--extra", "decision"]);
        let status = tokio::select! {
            result = command.status() => result.map_err(|_| setup_error("decision_dependencies_missing"))?,
            _ = cancel.wait_for(|cancelled| *cancelled) => return Err(AsrError::Cancelled),
        };
        if !status.success() {
            return Err(setup_error("decision_dependencies_missing"));
        }
        Ok(())
    }
}

/// 64-bit FNV-1a over length-prefixed fields. It detects accidental changes
/// to the sync inputs; it is not meant to resist deliberate tampering with
/// the user's own data directory.
struct Fnv1a(u64);

impl Default for Fnv1a {
    fn default() -> Self {
        Self(0xcbf2_9ce4_8422_2325)
    }
}

impl Fnv1a {
    fn write(&mut self, bytes: &[u8]) {
        for byte in bytes {
            self.0 ^= u64::from(*byte);
            self.0 = self.0.wrapping_mul(0x0000_0100_0000_01b3);
        }
    }

    fn field(&mut self, bytes: &[u8]) {
        self.write(&(bytes.len() as u64).to_le_bytes());
        self.write(bytes);
    }

    fn finish(&self) -> u64 {
        self.0
    }
}

fn setup_error(code: &str) -> AsrError {
    AsrError::Worker(WorkerError {
        code: code.into(),
        message: "Worker environment setup failed".into(),
    })
}

pub(crate) fn install(resource_dir: &Path, local_data: &Path) {
    let source = resource_dir.join("worker");
    // Preserve packaged identity even when an installation is incomplete.
    // Missing files must produce setup guidance, never select another checkout.
    if source.is_dir() {
        let _ = BUNDLED.set(Provision {
            source,
            environment: local_data
                .join("worker-runtime")
                .join(env!("CARGO_PKG_VERSION")),
            vibevoice: false,
            uv: environment::value_os("ASR_UV")
                .unwrap_or_else(|| "uv".into())
                .into(),
        });
    }
}

pub(crate) fn bundled() -> Option<&'static Provision> {
    BUNDLED.get()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bundled_setup_keeps_the_environment_outside_read_only_resources() {
        let provision = Provision {
            source: PathBuf::from("resources/worker"),
            environment: PathBuf::from("user-data/runtime"),
            vibevoice: true,
            uv: "uv".into(),
        };
        let command = provision.command();
        let command = command.as_std();
        assert_eq!(command.get_current_dir(), Some(provision.source.as_path()));
        let args: Vec<_> = command.get_args().collect();
        assert!(args.windows(2).any(|pair| pair == ["--extra", "vibevoice"]));
        assert!(args.iter().any(|arg| *arg == "--inexact"));
        assert!(!args.iter().any(|arg| *arg == "decision"));
        assert!(args.iter().any(|arg| *arg == "--frozen"));
        assert_eq!(
            command
                .get_envs()
                .find(|(key, _)| *key == "UV_PROJECT_ENVIRONMENT")
                .unwrap()
                .1,
            Some(provision.environment.as_os_str())
        );
        assert!(provision.python().starts_with(&provision.environment));
        assert!(!provision.python().starts_with(&provision.source));
    }

    /// A provision whose project files and environment exist on disk, as
    /// after a sync, without running uv.
    fn synced_layout(directory: &Path) -> Provision {
        let source = directory.join("worker");
        std::fs::create_dir_all(&source).unwrap();
        std::fs::write(source.join("uv.lock"), "lock v1").unwrap();
        std::fs::write(source.join("pyproject.toml"), "[project]").unwrap();
        std::fs::write(source.join(".python-version"), "3.12").unwrap();
        let provision = Provision {
            source,
            environment: directory.join("runtime"),
            vibevoice: false,
            uv: directory.join("missing-uv"),
        };
        std::fs::create_dir_all(provision.python().parent().unwrap()).unwrap();
        std::fs::write(provision.python(), b"interpreter").unwrap();
        std::fs::write(provision.environment.join("pyvenv.cfg"), "home = a").unwrap();
        provision
    }

    #[test]
    fn a_recorded_sync_matches_until_an_input_changes() {
        let directory = tempfile::tempdir().unwrap();
        let provision = synced_layout(directory.path());
        assert!(!provision.is_synced(), "no sync was recorded yet");
        provision.record_sync();
        assert!(provision.is_synced());
        assert!(!provision
            .environment
            .join(format!("{SYNC_MARKER}.partial"))
            .exists());

        let vibevoice = Provision {
            vibevoice: true,
            ..provision.clone()
        };
        assert!(!vibevoice.is_synced(), "other extras need their own sync");

        let changes: [(&Path, &str); 4] = [
            (&provision.source.join("uv.lock"), "lock v2"),
            (&provision.source.join("pyproject.toml"), "[project]\n"),
            (&provision.source.join(".python-version"), "3.13"),
            (&provision.environment.join("pyvenv.cfg"), "home = b"),
        ];
        for (path, changed) in changes {
            let original = std::fs::read(path).unwrap();
            std::fs::write(path, changed).unwrap();
            assert!(!provision.is_synced(), "{}", path.display());
            std::fs::write(path, original).unwrap();
            assert!(provision.is_synced(), "{}", path.display());
        }

        std::fs::remove_file(provision.python()).unwrap();
        assert!(!provision.is_synced(), "the interpreter must exist");
        std::fs::write(provision.python(), b"interpreter").unwrap();
        assert!(provision.is_synced());
        provision.forget_sync();
        assert!(!provision.is_synced());
    }

    #[test]
    fn a_sync_with_unreadable_inputs_is_never_recorded() {
        let directory = tempfile::tempdir().unwrap();
        let provision = synced_layout(directory.path());
        std::fs::remove_file(provision.source.join("uv.lock")).unwrap();
        provision.record_sync();
        assert!(!provision.environment.join(SYNC_MARKER).exists());
        assert!(!provision.is_synced());
    }

    #[tokio::test]
    async fn a_failed_sync_forgets_the_previous_one() {
        let directory = tempfile::tempdir().unwrap();
        let provision = synced_layout(directory.path());
        provision.record_sync();
        assert!(provision.is_synced_async().await);
        assert!(provision.prepare().await.is_err());
        assert!(!provision.is_synced_async().await);
    }

    #[tokio::test]
    async fn missing_uv_reports_setup_instructions_without_os_details() {
        let directory = tempfile::tempdir().unwrap();
        let provision = Provision {
            source: directory.path().into(),
            environment: directory.path().join("runtime"),
            vibevoice: false,
            uv: directory.path().join("missing-private-program"),
        };
        let error = provision.prepare().await.unwrap_err();
        assert!(
            matches!(&error, AsrError::Worker(error) if error.code == "worker_setup_unavailable")
        );
        assert!(!error.to_string().contains("missing-private-program"));
        assert!(!provision.environment.exists());
    }
}
