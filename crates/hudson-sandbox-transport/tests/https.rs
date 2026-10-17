//! Real public SDK against a local trusted-CA HTTPS protocol fixture; no VM.
use hudson_sandbox_transport::PreparedBody;
use sandbox_client::{models::*, requests::*, Client};
use std::{os::unix::fs::PermissionsExt, sync::Arc};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio_rustls::{rustls, TlsAcceptor};

fn private(path: &std::path::Path, bytes: &[u8]) {
    std::fs::write(path, bytes).unwrap();
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)).unwrap();
}
#[tokio::test]
async fn sdk_sends_authenticated_canonical_bodies_and_parses_admission() {
    for step in [0, 1, 2] {
        let _ = rustls::crypto::ring::default_provider().install_default();
        let cert = rcgen::generate_simple_self_signed(vec!["localhost".into()]).unwrap();
        let config = rustls::ServerConfig::builder()
            .with_no_client_auth()
            .with_single_cert(
                vec![cert.cert.der().clone()],
                rustls::pki_types::PrivateKeyDer::Pkcs8(
                    rustls::pki_types::PrivatePkcs8KeyDer::from(cert.signing_key.serialize_der()),
                ),
            )
            .unwrap();
        let acceptor = TlsAcceptor::from(Arc::new(config));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let server = tokio::spawn(async move {
            let (socket, _) = listener.accept().await.unwrap();
            let mut tls = acceptor.accept(socket).await.unwrap();
            let mut bytes = Vec::new();
            let mut chunk = [0u8; 1024];
            loop {
                let size = tls.read(&mut chunk).await.unwrap();
                assert!(size > 0);
                bytes.extend_from_slice(&chunk[..size]);
                if let Some(split) = bytes.windows(4).position(|w| w == b"\r\n\r\n") {
                    let headers = std::str::from_utf8(&bytes[..split]).unwrap();
                    let size: usize = headers
                        .lines()
                        .find_map(|line| {
                            line.to_ascii_lowercase()
                                .strip_prefix("content-length: ")
                                .map(str::to_owned)
                        })
                        .unwrap()
                        .parse()
                        .unwrap();
                    if bytes.len() >= split + 4 + size {
                        break;
                    }
                }
                assert!(bytes.len() < 65536);
            }
            let body = r#"{"sandbox_id":"sb_original","operation_id":"op_original","status":"queued","status_url":"/v1/operations/op_original"}"#;
            let response = format!("HTTP/1.1 202 Accepted\r\nContent-Type: application/json\r\nCache-Control: no-store\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}", body.len(), body);
            tls.write_all(response.as_bytes()).await.unwrap();
            tls.shutdown().await.unwrap();
            bytes
        });
        let directory = tempfile::tempdir().unwrap();
        private(&directory.path().join("ca.pem"), cert.cert.pem().as_bytes());
        private(&directory.path().join("credential.json"), br#"{"version":1,"project_id":"prj_fixture","name":"test","token":"fixture-only","created_at":1,"expires_at":4102444800}"#);
        let profile = directory.path().join("client.json");
        private(&profile, serde_json::to_vec(&serde_json::json!({"version":1,"endpoint":format!("https://localhost:{port}"),"credential_file":"credential.json","ca_file":"ca.pem"})).unwrap().as_slice());
        let client = Client::from_config(&profile).unwrap();
        let prepared = match step {
            0 => PreparedBody::Create(CreateRequest {
                image_digest: "sha256:fixture".into(),
                name: None,
                resources: RequestedResources {
                    vcpu: 1,
                    memory_mib: 512,
                    disk_mib: 1024,
                },
                correlation_id: Some("hudson-fence".into()),
            }),
            1 => PreparedBody::Execute(CommandInput {
                argv: vec!["/approved/agent".into()],
                env: Default::default(),
                cwd: "/workspace".into(),
                deadline_unix_ms: 123456789,
                output_limit: 4096,
            }),
            _ => PreparedBody::Destroy(DestroyRequest {
                correlation_id: Some("hudson-fence".into()),
            }),
        };
        let expected = prepared.canonical_json().unwrap();
        let (admitted, path) = match &prepared {
            PreparedBody::Create(body) => (
                client
                    .create_sandbox(CreateSandbox {
                        idempotency_key: "stable-key-000001",
                        body,
                    })
                    .await
                    .unwrap(),
                "/v1/sandboxes",
            ),
            PreparedBody::Execute(body) => (
                client
                    .execute_command(ExecuteCommand {
                        sandbox_id: "sb_original",
                        idempotency_key: "stable-key-000001",
                        body,
                    })
                    .await
                    .unwrap(),
                "/v1/sandboxes/sb_original/execute",
            ),
            PreparedBody::Destroy(body) => (
                client
                    .destroy_sandbox(DestroySandbox {
                        sandbox_id: "sb_original",
                        idempotency_key: "stable-key-000001",
                        body,
                    })
                    .await
                    .unwrap(),
                "/v1/sandboxes/sb_original/destroy",
            ),
        };
        assert_eq!(admitted.operation_id, "op_original");
        assert_eq!(admitted.status, "queued");
        let bytes = server.await.unwrap();
        let split = bytes.windows(4).position(|w| w == b"\r\n\r\n").unwrap();
        assert_eq!(&bytes[split + 4..], expected.as_bytes());
        let headers = std::str::from_utf8(&bytes[..split])
            .unwrap()
            .to_ascii_lowercase();
        assert!(headers.starts_with(&format!("post {path} http/1.1")));
        assert!(headers.contains("authorization: bearer fixture-only"));
        assert!(headers.contains("idempotency-key: stable-key-000001"));
    }
}
