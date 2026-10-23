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
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let endpoint = format!(
            "http://127.0.0.1:{}/provider",
            listener.local_addr().unwrap().port()
        );
        let status = status.to_owned();
        let body = body.to_string();
        let (send, captured) = oneshot::channel();
        let (release, receive) = mpsc::channel();
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
            let _ = send.send(captured);
            if hold {
                let _ = receive.recv_timeout(Duration::from_secs(5));
            }
            let response = format!("HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len());
            // A cancellation deliberately drops the connection before a reply.
            let _ = socket.write_all(response.as_bytes());
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
