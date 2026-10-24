use super::*;
use serde_json::{json, Value};
use std::{
    io::{Read, Write},
    net::TcpListener,
    sync::mpsc,
    thread,
    time::{Duration, Instant},
};
use tokio::sync::oneshot;

const STUB_KEY: &str = "synthetic-stub-key";

struct CapturedRequest {
    headers: String,
    body: Value,
}
struct Stub {
    endpoint: String,
    captured: oneshot::Receiver<CapturedRequest>,
    release: mpsc::Sender<()>,
    handler: thread::JoinHandle<()>,
}

impl Stub {
    fn new(status: &str, body: Value, hold: bool) -> Self {
        let attempts = if status.starts_with("429") || status.starts_with('5') {
            2
        } else {
            1
        };
        Self::responses(
            (0..attempts)
                .map(|_| (status.to_owned(), body.to_string()))
                .collect(),
            hold,
        )
    }

    fn responses(responses: Vec<(String, String)>, hold: bool) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let endpoint = format!(
            "http://127.0.0.1:{}/provider",
            listener.local_addr().unwrap().port()
        );
        let (send, captured) = oneshot::channel();
        let (release, receive) = mpsc::channel();
        let handler = thread::spawn(move || {
            let mut send = Some(send);
            for (status, body) in responses {
                let deadline = Instant::now() + Duration::from_secs(5);
                let mut socket = loop {
                    match listener.accept() {
                        Ok((socket, _)) => break socket,
                        Err(error)
                            if error.kind() == std::io::ErrorKind::WouldBlock
                                && Instant::now() < deadline =>
                        {
                            thread::sleep(Duration::from_millis(5))
                        }
                        Err(error) => panic!("stub accept failed: {error}"),
                    }
                };
                // Winsock may propagate the listener's nonblocking mode to the
                // accepted socket. The bounded reads below require blocking I/O.
                socket.set_nonblocking(false).unwrap();
                socket
                    .set_read_timeout(Some(Duration::from_secs(5)))
                    .unwrap();
                let mut request = Vec::new();
                let mut buffer = [0; 4096];
                let captured = loop {
                    let count = socket.read(&mut buffer).unwrap();
                    assert!(count > 0, "request ended before its body");
                    request.extend_from_slice(&buffer[..count]);
                    assert!(
                        request.len() <= 64 * 1024,
                        "test request exceeded its bound"
                    );
                    if let Some(end) = request.windows(4).position(|window| window == b"\r\n\r\n") {
                        let headers = String::from_utf8(request[..end].to_vec())
                            .unwrap()
                            .to_lowercase();
                        let length = headers
                            .lines()
                            .find_map(|line| line.strip_prefix("content-length:"))
                            .unwrap()
                            .trim()
                            .parse::<usize>()
                            .unwrap();
                        if request.len() >= end + 4 + length {
                            break CapturedRequest {
                                headers,
                                body: serde_json::from_slice(&request[end + 4..end + 4 + length])
                                    .unwrap(),
                            };
                        }
                    }
                };
                if let Some(send) = send.take() {
                    let _ = send.send(captured);
                }
                if hold {
                    let _ = receive.recv_timeout(Duration::from_secs(5));
                }
                let response = format!("HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len());
                // A cancellation deliberately drops the connection before a reply.
                let _ = socket.write_all(response.as_bytes());
            }
        });
        Self {
            endpoint,
            captured,
            release,
            handler,
        }
    }
}

fn settings(provider: &str) -> Settings {
    Settings {
        correction_provider: provider.into(),
        cloud_consents: vec![provider.into()],
        ..Settings::default()
    }
}

fn success(provider: &str) -> Value {
    if provider == "openai" {
        json!({"output":[{"type":"message","content":[{"type":"output_text","text":" fixed text "}]}]})
    } else {
        json!({"steps":[{"type":"model_output","content":[{"type":"text","text":" fixed text "}]}]})
    }
}

#[tokio::test]
async fn transient_cloud_failures_retry_once_before_any_output() {
    for provider in ["openai", "gemini"] {
        for status in ["429 Too Many Requests", "503 Service Unavailable"] {
            let stub = Stub::responses(
                vec![
                    (
                        status.into(),
                        json!({"error":{"message":"transient"}}).to_string(),
                    ),
                    ("200 OK".into(), success(provider).to_string()),
                ],
                false,
            );
            let (_sender, cancel) = watch::channel(false);
            let mut deltas = Vec::new();
            let result = request_text_with_access(
                &settings(provider),
                TextRequest {
                    input: "text",
                    instruction: "instruction",
                    trim_output: true,
                    budget: OutputBudget::AskText,
                },
                cancel,
                |delta| deltas.push(delta.to_owned()),
                ProviderAccess::Stub {
                    endpoint: stub.endpoint,
                    api_key: STUB_KEY.into(),
                    client: None,
                },
            )
            .await
            .unwrap();
            assert_eq!(result, "fixed text");
            assert_eq!(deltas, [" fixed text "]);
            stub.captured.await.unwrap();
            stub.handler.join().unwrap();
        }
    }
}

#[tokio::test]
async fn a_long_server_backoff_is_not_retried_early() {
    let stub = Stub::responses(
        vec![(
            "429 Too Many Requests\r\nRetry-After: 120".into(),
            json!({"error":{"message":"wait"}}).to_string(),
        )],
        false,
    );
    let (_sender, cancel) = watch::channel(false);
    let error = request_text_with_access(
        &settings("openai"),
        TextRequest {
            input: "text",
            instruction: "instruction",
            trim_output: true,
            budget: OutputBudget::AskText,
        },
        cancel,
        |_| {},
        ProviderAccess::Stub {
            endpoint: stub.endpoint,
            api_key: STUB_KEY.into(),
            client: None,
        },
    )
    .await
    .unwrap_err();
    assert_eq!(error.failure_code(), "ai_rate_limited");
    stub.captured.await.unwrap();
    stub.handler.join().unwrap();
}

#[tokio::test]
async fn a_cloud_stream_outlasts_its_idle_period_while_bytes_keep_arriving() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let endpoint = format!(
        "http://127.0.0.1:{}/provider",
        listener.local_addr().unwrap().port()
    );
    let handler = thread::spawn(move || {
        let (mut socket, _) = listener.accept().unwrap();
        socket
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        let mut request = [0; 8192];
        assert!(socket.read(&mut request).unwrap() > 0);
        socket
            .write_all(
                b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\n",
            )
            .unwrap();
        for _ in 0..8 {
            socket
                .write_all(b"data: {\"type\":\"response.output_text.delta\",\"delta\":\"x\"}\n\n")
                .unwrap();
            thread::sleep(Duration::from_millis(50));
        }
        socket
            .write_all(b"data: {\"type\":\"response.completed\"}\n\n")
            .unwrap();
    });
    let (_sender, cancel) = watch::channel(false);
    let started = Instant::now();
    let result = request_text_with_access(
        &settings("openai"),
        TextRequest {
            input: "text",
            instruction: "instruction",
            trim_output: true,
            budget: OutputBudget::AskText,
        },
        cancel,
        |_| {},
        ProviderAccess::Stub {
            endpoint,
            api_key: STUB_KEY.into(),
            client: Some(cloud_client_with(Duration::from_millis(200), true).unwrap()),
        },
    )
    .await
    .unwrap();
    assert_eq!(result, "xxxxxxxx");
    assert!(started.elapsed() > Duration::from_millis(300));
    handler.join().unwrap();
}

#[tokio::test]
async fn a_failed_cloud_stream_is_not_retried_after_a_visible_delta() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let endpoint = format!(
        "http://127.0.0.1:{}/provider",
        listener.local_addr().unwrap().port()
    );
    let handler = thread::spawn(move || {
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut socket = loop {
            match listener.accept() {
                Ok((socket, _)) => break socket,
                Err(error)
                    if error.kind() == std::io::ErrorKind::WouldBlock
                        && Instant::now() < deadline =>
                {
                    thread::sleep(Duration::from_millis(5))
                }
                Err(error) => panic!("accept failed: {error}"),
            }
        };
        socket.set_nonblocking(false).unwrap();
        socket
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        let mut request = [0; 8192];
        assert!(socket.read(&mut request).unwrap() > 0);
        socket.write_all(b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: 10000\r\nConnection: close\r\n\r\ndata: {\"type\":\"response.output_text.delta\",\"delta\":\"partial\"}\n\n").unwrap();
        drop(socket);
        let deadline = Instant::now() + Duration::from_millis(600);
        while Instant::now() < deadline {
            if listener.accept().is_ok() {
                return false;
            }
            thread::sleep(Duration::from_millis(5));
        }
        true
    });
    let (_sender, cancel) = watch::channel(false);
    let mut deltas = Vec::new();
    let error = request_text_with_access(
        &settings("openai"),
        TextRequest {
            input: "text",
            instruction: "instruction",
            trim_output: true,
            budget: OutputBudget::AskText,
        },
        cancel,
        |delta| deltas.push(delta.to_owned()),
        ProviderAccess::Stub {
            endpoint,
            api_key: STUB_KEY.into(),
            client: None,
        },
    )
    .await
    .unwrap_err();
    assert!(matches!(error, CorrectionError::Request(_)));
    assert_eq!(deltas, ["partial"]);
    assert!(
        handler.join().unwrap(),
        "a visible partial result must not be replayed"
    );
}

#[tokio::test]
async fn both_cloud_transports_send_auth_and_task_fields_and_parse_success() {
    for provider in ["openai", "gemini"] {
        let stub = Stub::new("200 OK", success(provider), false);
        let (_cancel, cancel) = watch::channel(false);
        let mut updates = Vec::new();
        let result = request_text_with_access(
            &settings(provider),
            TextRequest {
                input: "こんにちは",
                instruction: "fixed instruction",
                trim_output: true,
                budget: OutputBudget::Edit,
            },
            cancel,
            |text| updates.push(text.to_owned()),
            ProviderAccess::Stub {
                endpoint: stub.endpoint,
                api_key: STUB_KEY.into(),
                client: None,
            },
        )
        .await;
        assert_eq!(result.unwrap(), "fixed text");
        let request = stub.captured.await.unwrap();
        let auth = if provider == "openai" {
            format!("authorization: bearer {STUB_KEY}")
        } else {
            format!("x-goog-api-key: {STUB_KEY}")
        };
        assert!(request.headers.contains(&auth));
        assert_eq!(request.body["input"], "こんにちは");
        assert_eq!(
            request.body[if provider == "openai" {
                "instructions"
            } else {
                "system_instruction"
            }],
            "fixed instruction"
        );
        assert_eq!(request.body["stream"], true);
        assert_eq!(request.body["store"], false);
        assert!(!request.body.to_string().contains(STUB_KEY));
        assert!(!updates.is_empty());
        stub.handler.join().unwrap();
    }
}

#[tokio::test]
async fn cloud_http_errors_are_classified_and_do_not_echo_the_sent_key() {
    for provider in ["openai", "gemini"] {
        for (status, code) in [
            ("401 Unauthorized", "ai_invalid_api_key"),
            ("429 Too Many Requests", "ai_rate_limited"),
        ] {
            let stub = Stub::new(
                status,
                json!({"error":{"message":format!("request key was {STUB_KEY}")}}),
                false,
            );
            let (_cancel, cancel) = watch::channel(false);
            let error = request_text_with_access(
                &settings(provider),
                TextRequest {
                    input: "text",
                    instruction: "fixed instruction",
                    trim_output: true,
                    budget: OutputBudget::Transcript,
                },
                cancel,
                |_| {},
                ProviderAccess::Stub {
                    endpoint: stub.endpoint,
                    api_key: STUB_KEY.into(),
                    client: None,
                },
            )
            .await
            .unwrap_err();
            assert_eq!(error.failure_code(), code);
            assert!(!error.to_string().contains(STUB_KEY));
            assert!(!error.user_message().contains(STUB_KEY));
            stub.captured.await.unwrap();
            stub.handler.join().unwrap();
        }
    }
}

#[tokio::test]
async fn both_cloud_transports_cancel_a_request_waiting_for_a_response() {
    for provider in ["openai", "gemini"] {
        let stub = Stub::new("200 OK", success(provider), true);
        let (cancel, receiver) = watch::channel(false);
        let endpoint = stub.endpoint;
        let task = tokio::spawn(async move {
            request_text_with_access(
                &settings(provider),
                TextRequest {
                    input: "text",
                    instruction: "fixed instruction",
                    trim_output: true,
                    budget: OutputBudget::Transcript,
                },
                receiver,
                |_| {},
                ProviderAccess::Stub {
                    endpoint,
                    api_key: STUB_KEY.into(),
                    client: None,
                },
            )
            .await
        });
        stub.captured.await.unwrap();
        cancel.send_replace(true);
        assert!(matches!(
            tokio::time::timeout(Duration::from_secs(2), task)
                .await
                .unwrap()
                .unwrap(),
            Err(CorrectionError::Cancelled)
        ));
        let _ = stub.release.send(());
        stub.handler.join().unwrap();
    }
}
