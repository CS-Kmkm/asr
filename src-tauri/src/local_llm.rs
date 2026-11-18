//! Explicit, owned llama-server lifecycle. No shell, installer or model download.
use crate::types::Settings;
use serde::Serialize;
use std::{
    path::Path,
    process::Stdio,
    sync::{
        atomic::{AtomicU64, Ordering},
        OnceLock,
    },
    time::{Duration, Instant},
};
use tokio::{
    process::{Child, Command},
    sync::{watch, Mutex},
};

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ServerStatus {
    state: &'static str,
    pid: Option<u32>,
    ready_ms: Option<u64>,
}

#[derive(Default)]
struct Manager {
    child: Option<Child>,
    identity: Option<String>,
    ready_ms: Option<u64>,
}
static STOP_GENERATION: AtomicU64 = AtomicU64::new(0);
fn manager() -> &'static Mutex<Manager> {
    static MANAGER: OnceLock<Mutex<Manager>> = OnceLock::new();
    MANAGER.get_or_init(|| Mutex::new(Manager::default()))
}

pub(crate) fn validate(settings: &Settings) -> Result<(), String> {
    let cfg = &settings.local_server;
    if !(512..=32768).contains(&cfg.context_size) || cfg.gpu_layers > 999 {
        return Err("Local server context or GPU layer setting is invalid.".into());
    }
    for path in [&cfg.executable_path, &cfg.model_path] {
        if path.len() > 32768
            || path.chars().any(char::is_control)
            || (!path.is_empty() && !Path::new(path).is_absolute())
        {
            return Err(
                "Local server paths must be absolute and contain no control characters.".into(),
            );
        }
    }
    if cfg.auto_start && (cfg.executable_path.is_empty() || cfg.model_path.is_empty()) {
        return Err(
            "Choose a llama-server executable and GGUF model before enabling automatic startup."
                .into(),
        );
    }
    Ok(())
}

fn address(settings: &Settings) -> Result<(String, u16), String> {
    let url = crate::correction::local_chat_completions_url(&settings.local_correction_base_url)
        .map_err(|_| "Local server needs a numeric loopback HTTP URL ending in /v1.".to_owned())?;
    if url.scheme() != "http" || url.path() != "/v1/chat/completions" {
        return Err("Local server needs a numeric loopback HTTP URL ending in /v1.".into());
    }
    let host = url.host_str().unwrap().trim_matches(['[', ']']).to_owned();
    Ok((host, url.port_or_known_default().unwrap()))
}

impl Manager {
    async fn stop(&mut self) {
        if let Some(mut child) = self.child.take() {
            let _ = child.kill().await;
            let _ = child.wait().await;
        }
        self.identity = None;
        self.ready_ms = None;
    }
    fn status(&mut self) -> ServerStatus {
        let running = self
            .child
            .as_mut()
            .is_some_and(|child| matches!(child.try_wait(), Ok(None)));
        if !running {
            self.child = None;
            self.identity = None;
            self.ready_ms = None;
        }
        ServerStatus {
            state: if running && self.ready_ms.is_some() {
                "ready"
            } else if running {
                "starting"
            } else {
                "stopped"
            },
            pid: self.child.as_ref().and_then(Child::id),
            ready_ms: self.ready_ms,
        }
    }
    async fn start(
        &mut self,
        settings: &Settings,
        cancel: &watch::Receiver<bool>,
        generation: u64,
    ) -> Result<ServerStatus, String> {
        validate(settings)?;
        let cfg = &settings.local_server;
        let (host, port) = address(settings)?;
        let executable = Path::new(&cfg.executable_path);
        let model = Path::new(&cfg.model_path);
        if !executable.is_file()
            || !matches!(
                executable.file_name().and_then(|name| name.to_str()),
                Some("llama-server.exe" | "llama-server")
            )
            || !model.is_file()
            || model
                .extension()
                .and_then(|value| value.to_str())
                .is_none_or(|value| !value.eq_ignore_ascii_case("gguf"))
        {
            return Err("Choose an existing llama-server executable and GGUF model. No files are downloaded automatically.".into());
        }
        let identity = serde_json::to_string(&(
            &cfg.executable_path,
            &cfg.model_path,
            cfg.context_size,
            cfg.gpu_layers,
            &settings.local_correction_base_url,
            &settings.local_correction_model,
        ))
        .map_err(|_| "Local server configuration is invalid.")?;
        if self.status().state == "ready" {
            if self.identity.as_deref() == Some(&identity) {
                return Ok(self.status());
            }
            return Err("Stop the owned local server before changing its configuration.".into());
        }
        if *cancel.borrow() || STOP_GENERATION.load(Ordering::SeqCst) != generation {
            return Err("Local server startup was cancelled.".into());
        }
        let socket: std::net::SocketAddr = format!(
            "{}:{port}",
            if host.contains(':') {
                format!("[{host}]")
            } else {
                host.clone()
            }
        )
        .parse()
        .map_err(|_| "Local server address is invalid.")?;
        // Never attach to or stop an externally operated server on this port.
        let probe = std::net::TcpListener::bind(socket).map_err(|_| "The local server port is already in use. Choose another port or use the existing server without automatic startup.")?;
        drop(probe);
        let client = reqwest::Client::builder()
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .timeout(Duration::from_millis(500))
            .build()
            .map_err(|_| "Local server readiness checks are unavailable.")?;
        let mut command = Command::new(executable);
        command
            .args([
                "--model",
                &cfg.model_path,
                "--host",
                &host,
                "--port",
                &port.to_string(),
                "--ctx-size",
                &cfg.context_size.to_string(),
                "--n-gpu-layers",
                &cfg.gpu_layers.to_string(),
                "--parallel",
                "1",
                "--alias",
                settings.local_correction_model.trim(),
            ])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .kill_on_drop(true);
        #[cfg(target_os = "windows")]
        command.creation_flags(0x08000000);
        let child = command.spawn().map_err(|_| {
            "The local server could not start. Check the executable and its runtime libraries."
        })?;
        self.child = Some(child);
        self.identity = Some(identity);
        let started = Instant::now();
        let health = format!("http://{}/health", socket);
        let result = loop {
            if *cancel.borrow() || STOP_GENERATION.load(Ordering::SeqCst) != generation {
                break Err("Local server startup was cancelled.".to_owned());
            }
            if self
                .child
                .as_mut()
                .is_none_or(|child| !matches!(child.try_wait(), Ok(None)))
            {
                break Err("The local server exited before becoming ready. Check the model, GPU memory and runtime libraries.".to_owned());
            }
            if let Ok(response) = client.get(&health).send().await {
                if response.status().is_success()
                    && response
                        .json::<serde_json::Value>()
                        .await
                        .ok()
                        .is_some_and(|value| value["status"] == "ok")
                    && self
                        .child
                        .as_mut()
                        .is_some_and(|child| matches!(child.try_wait(), Ok(None)))
                {
                    self.ready_ms = Some(started.elapsed().as_millis() as u64);
                    break Ok(self.status());
                }
            }
            if started.elapsed() >= Duration::from_secs(120) {
                break Err("The local server did not become ready in time. Check the model and available memory.".to_owned());
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        };
        if result.is_err() {
            self.stop().await;
        }
        result
    }
}

pub(crate) async fn start(
    settings: &Settings,
    mut cancel: watch::Receiver<bool>,
) -> Result<ServerStatus, String> {
    let generation = STOP_GENERATION.load(Ordering::SeqCst);
    let mut guard = tokio::select! {
        guard = manager().lock() => guard,
        _ = async {
            loop {
                if *cancel.borrow() { return; }
                if cancel.changed().await.is_err() { std::future::pending::<()>().await; }
            }
        } => return Err("Local server startup was cancelled.".into()),
    };
    guard.start(settings, &cancel, generation).await
}
pub(crate) async fn status() -> ServerStatus {
    manager().lock().await.status()
}
pub(crate) async fn stop() {
    STOP_GENERATION.fetch_add(1, Ordering::SeqCst);
    manager().lock().await.stop().await;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn managed_server_requires_numeric_loopback_and_exact_api_path() {
        let mut settings = Settings::default();
        for url in ["http://127.0.0.1:8123/v1", "http://[::1]:8123/v1"] {
            settings.local_correction_base_url = url.into();
            assert!(address(&settings).is_ok());
        }
        for url in [
            "http://localhost:8123/v1",
            "https://127.0.0.1/v1",
            "http://192.168.1.2/v1",
            "http://127.0.0.1/other",
        ] {
            settings.local_correction_base_url = url.into();
            assert!(address(&settings).is_err(), "{url}");
        }
        settings.local_server.auto_start = true;
        assert!(validate(&settings).is_err());
        settings.local_server.auto_start = false;
        settings.local_server.context_size = 1;
        assert!(validate(&settings).is_err());
    }

    #[tokio::test]
    async fn missing_runtime_is_recoverable_and_stop_owns_no_external_process() {
        let mut manager = Manager::default();
        let (_sender, cancel) = watch::channel(false);
        assert!(manager
            .start(
                &Settings::default(),
                &cancel,
                STOP_GENERATION.load(Ordering::SeqCst)
            )
            .await
            .is_err());
        assert_eq!(manager.status().state, "stopped");
        assert!(manager.child.is_none());
        manager.stop().await;
        assert!(manager.status().pid.is_none());
    }
}
