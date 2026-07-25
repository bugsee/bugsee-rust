//
//  live_capture.rs
//  bugsee-reqwest
//
//  Copyright © 2026 Bugsee. All rights reserved.
//
//! Phase 4 network capture E2E: a real request through the middleware against a
//! local server is captured into network.json (before + complete, shared id,
//! sanitized URL).

use std::io::{Cursor, Read, Write};
use std::net::TcpListener;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use bugsee::core::MockTransport;
use bugsee::{Bugsee, LaunchOptions};
use bugsee_reqwest::BugseeMiddleware;
use reqwest_middleware::ClientBuilder;
use serde_json::Value;
use zip::ZipArchive;

struct TempDir {
    path: PathBuf,
}
impl TempDir {
    fn new() -> Self {
        let mut path = std::env::temp_dir();
        path.push(format!("bugsee-net-{}", bugsee::core::util::random_hex(8)));
        std::fs::create_dir_all(&path).unwrap();
        TempDir { path }
    }
}
impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

#[tokio::test]
async fn captures_request_and_response_into_network_json() {
    let dir = TempDir::new();
    let mock = Arc::new(MockTransport::default());
    let _guard = Bugsee::launch_with(
        LaunchOptions::new("T")
            .data_dir(&dir.path)
            .with_transport(mock.clone()),
    )
    .unwrap();

    // A one-shot local HTTP server.
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let server = std::thread::spawn(move || {
        if let Ok((mut stream, _)) = listener.accept() {
            let mut buf = [0u8; 2048];
            let _ = stream.read(&mut buf);
            let _ = stream.write_all(
                b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nContent-Type: text/plain\r\n\r\nhi",
            );
            let _ = stream.flush();
        }
    });

    let client = ClientBuilder::new(reqwest::Client::new())
        .with(BugseeMiddleware::new())
        .build();
    let resp = client
        .get(format!("http://{addr}/v1/users?token=supersecret&page=2"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let _ = resp.text().await;
    server.join().unwrap();

    Bugsee::upload();
    assert!(Bugsee::flush(Duration::from_secs(5)));

    let bundles = mock.uploaded_bundles.lock().unwrap();
    assert_eq!(bundles.len(), 1);
    let mut zip = ZipArchive::new(Cursor::new(bundles[0].clone())).unwrap();
    let net_name = (0..zip.len())
        .map(|i| zip.by_index(i).unwrap().name().to_string())
        .find(|n| n.ends_with(".network.json"))
        .expect("network.json present");
    let mut nbytes = Vec::new();
    zip.by_name(&net_name)
        .unwrap()
        .read_to_end(&mut nbytes)
        .unwrap();
    let net: Value = serde_json::from_slice(&nbytes).unwrap();
    let events = net["events"].as_array().unwrap();

    // A before entry and a complete entry sharing one id.
    assert!(events.len() >= 2, "before + complete entries");
    let id0 = events[0]["id"].as_str().unwrap();
    assert!(events.iter().all(|e| e["id"] == id0), "shared request id");

    let stages: Vec<&str> = events.iter().map(|e| e["type"].as_str().unwrap()).collect();
    assert!(stages.contains(&"before"));
    assert!(stages.contains(&"complete"));

    // URL sanitized: token redacted, other params preserved.
    let url = events[0]["url"].as_str().unwrap();
    assert!(url.contains("page=2"), "non-sensitive param kept: {url}");
    assert!(!url.contains("supersecret"), "token redacted: {url}");

    // The complete entry carries the 200 status and mechanism.
    let complete = events.iter().find(|e| e["type"] == "complete").unwrap();
    assert_eq!(complete["status"], 200);
    assert_eq!(complete["mechanism"], "reqwest");
}
