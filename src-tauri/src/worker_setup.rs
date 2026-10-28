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

    pub(crate) async fn prepare(&self) -> Result<(), AsrError> {
        // Align base requirements on each spawn while retaining optional
        // packages the user explicitly prepared for another feature.
        let status = self
            .command()
            .status()
            .await
            .map_err(|_| setup_error("worker_setup_unavailable"))?;
        if !status.success() || !self.python().is_file() {
            return Err(setup_error("worker_setup_failed"));
        }
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
