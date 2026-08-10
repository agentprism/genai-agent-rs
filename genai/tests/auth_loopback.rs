//! Tests for the optional loopback redirect-capture helper (feature = `loopback`).
#![cfg(all(feature = "auth", feature = "loopback"))]

use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::sync::oneshot;

use genai::auth::loopback::{capture_redirect_with, LoopbackConfig};

async fn get(addr: std::net::SocketAddr, target: &str) -> String {
    let mut stream = TcpStream::connect(addr).await.unwrap();
    let request = format!("GET {target} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n");
    stream.write_all(request.as_bytes()).await.unwrap();
    stream.flush().await.unwrap();
    let mut buf = Vec::new();
    stream.read_to_end(&mut buf).await.unwrap();
    String::from_utf8_lossy(&buf).to_string()
}

#[tokio::test]
async fn captures_code_on_valid_redirect() {
    let config = LoopbackConfig {
        host: "127.0.0.1".to_string(),
        port: 0, // ephemeral
        path: "/auth/callback".to_string(),
        timeout: Duration::from_secs(10),
    };

    let (tx, rx) = oneshot::channel();
    let server = tokio::spawn(async move {
        capture_redirect_with(&config, "state-xyz", |addr| {
            let _ = tx.send(addr);
        })
        .await
    });

    let addr = rx.await.unwrap();

    // A request to the wrong path first: should 404 but not end the wait.
    let resp = get(addr, "/nope").await;
    assert!(resp.starts_with("HTTP/1.1 404"), "{resp}");

    // A state mismatch: 400 but keeps waiting.
    let resp = get(addr, "/auth/callback?code=c&state=wrong").await;
    assert!(resp.starts_with("HTTP/1.1 400"), "{resp}");

    // The valid redirect completes the wait.
    let resp = get(addr, "/auth/callback?code=THE-CODE&state=state-xyz").await;
    assert!(resp.starts_with("HTTP/1.1 200"), "{resp}");
    assert!(resp.contains("Authentication complete"), "{resp}");

    let code = server.await.unwrap().unwrap();
    assert_eq!(code, "THE-CODE");
}
