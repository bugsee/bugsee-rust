//
//  http_transport.rs
//  bugsee
//
//  Copyright © 2026 Bugsee. All rights reserved.
//
//! Integration coverage for the shipping `HttpTransport` (feature `net`). A
//! local `TcpListener` stands in for the Bugsee API and drives the 3-step
//! delivery protocol — register session → create issue → PUT bundle — plus the
//! app/HTTP error-code mapping. No network access; everything is on 127.0.0.1.

#![cfg(feature = "net")]

use std::collections::HashMap;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::thread::{self, JoinHandle};

use bugsee::transport::HttpTransport;
use bugsee::{Transport, TransportError};
use serde_json::Value;

/// A parsed HTTP request as seen by the mock server.
struct Request {
    method: String,
    /// Request target (path + query).
    target: String,
    /// Header names lowercased.
    headers: HashMap<String, String>,
    body: Vec<u8>,
}

fn find(hay: &[u8], needle: &[u8]) -> Option<usize> {
    hay.windows(needle.len()).position(|w| w == needle)
}

/// Read one full HTTP/1.1 request (headers + Content-Length body) off `stream`.
fn read_request(stream: &mut TcpStream) -> Request {
    let mut buf = Vec::new();
    let mut tmp = [0u8; 2048];
    let header_end = loop {
        if let Some(pos) = find(&buf, b"\r\n\r\n") {
            break pos + 4;
        }
        let n = stream.read(&mut tmp).expect("read request headers");
        if n == 0 {
            break buf.len();
        }
        buf.extend_from_slice(&tmp[..n]);
    };

    let header_text = String::from_utf8_lossy(&buf[..header_end]).into_owned();
    let mut lines = header_text.split("\r\n");
    let mut request_line = lines.next().unwrap_or("").split_whitespace();
    let method = request_line.next().unwrap_or("").to_string();
    let target = request_line.next().unwrap_or("").to_string();

    let mut headers = HashMap::new();
    for line in lines {
        if line.is_empty() {
            continue;
        }
        if let Some((k, v)) = line.split_once(':') {
            headers.insert(k.trim().to_ascii_lowercase(), v.trim().to_string());
        }
    }

    let content_length: usize = headers
        .get("content-length")
        .and_then(|v| v.parse().ok())
        .unwrap_or(0);
    let mut body = buf[header_end..].to_vec();
    while body.len() < content_length {
        let n = stream.read(&mut tmp).expect("read request body");
        if n == 0 {
            break;
        }
        body.extend_from_slice(&tmp[..n]);
    }

    Request {
        method,
        target,
        headers,
        body,
    }
}

/// Build a raw HTTP/1.1 response. `Connection: close` forces one request per
/// TCP connection so the accept loop sees each of ureq's calls separately.
fn http_response(status: &str, body: &[u8]) -> Vec<u8> {
    let mut resp = format!(
        "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    )
    .into_bytes();
    resp.extend_from_slice(body);
    resp
}

/// Serve exactly `num_conns` connections on `listener`, replying per `responder`,
/// and return the recorded requests once done.
fn serve(
    listener: TcpListener,
    num_conns: usize,
    responder: impl Fn(&Request) -> Vec<u8> + Send + 'static,
) -> JoinHandle<Vec<Request>> {
    thread::spawn(move || {
        let mut recorded = Vec::new();
        for _ in 0..num_conns {
            let (mut stream, _) = listener.accept().expect("accept");
            let req = read_request(&mut stream);
            let resp = responder(&req);
            stream.write_all(&resp).expect("write response");
            let _ = stream.flush();
            recorded.push(req);
        }
        recorded
    })
}

#[test]
fn three_step_delivery_protocol() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let base = format!("http://127.0.0.1:{port}");
    let presigned = format!("http://127.0.0.1:{port}/uploads/presigned-123");
    let presigned_for_server = presigned.clone();

    let handle = serve(listener, 3, move |req| {
        if req.target.starts_with("/sessions") {
            http_response(
                "200 OK",
                br#"{"ok":true,"result":{"access_token":"access-tok-42"},"error":{"code":0}}"#,
            )
        } else if req.target.starts_with("/issues") {
            let body = format!(
                r#"{{"ok":true,"result":{{"endpoint":"{presigned_for_server}"}},"error":{{"code":0}}}}"#
            );
            http_response("200 OK", body.as_bytes())
        } else {
            // PUT upload — accept and 200.
            http_response("200 OK", b"{}")
        }
    });

    let transport = HttpTransport::new(Some(base));

    // Step 1: register_session parses result.access_token.
    let token = transport
        .register_session("APP_TOKEN", br#"{"os":"linux","app":"demo"}"#)
        .expect("register_session ok");
    assert_eq!(token, "access-tok-42");

    // Step 2: create_issue parses result.endpoint.
    let endpoint = transport
        .create_issue("APP_TOKEN", Some(&token), br#"{"type":"crash"}"#)
        .expect("create_issue ok");
    assert_eq!(endpoint, presigned);

    // Step 3: upload_bundle PUTs the raw bytes to the presigned URL.
    let bundle = b"RAW-ZIP-BYTES-\x00\x01\x02";
    transport
        .upload_bundle(&endpoint, bundle)
        .expect("upload_bundle ok");

    let reqs = handle.join().unwrap();
    assert_eq!(reqs.len(), 3);

    // --- register_session request ---
    let s = &reqs[0];
    assert_eq!(s.method, "POST");
    assert!(
        s.target.starts_with("/sessions"),
        "posts to /sessions, got {}",
        s.target
    );
    assert_eq!(
        s.headers.get("x-client-type").map(String::as_str),
        Some("rust"),
        "sends x-client-type: rust"
    );
    let sbody: Value = serde_json::from_slice(&s.body).unwrap();
    assert_eq!(sbody["app_token"], "APP_TOKEN");
    assert_eq!(
        sbody["environment"]["os"], "linux",
        "environment json is embedded"
    );

    // --- create_issue request ---
    let i = &reqs[1];
    assert_eq!(i.method, "POST");
    assert!(
        i.target.starts_with("/issues"),
        "posts to /issues, got {}",
        i.target
    );
    assert_eq!(
        i.headers.get("x-client-type").map(String::as_str),
        Some("rust")
    );
    let ibody: Value = serde_json::from_slice(&i.body).unwrap();
    assert_eq!(ibody["type"], "crash", "request.json body forwarded");
    assert_eq!(
        ibody["app_token"], "APP_TOKEN",
        "app_token merged into body"
    );
    assert_eq!(
        ibody["access_token"], "access-tok-42",
        "access_token merged into body"
    );

    // --- upload_bundle request ---
    let u = &reqs[2];
    assert_eq!(u.method, "PUT");
    assert!(
        u.target.starts_with("/uploads/presigned-123"),
        "PUT to presigned URL, got {}",
        u.target
    );
    assert_eq!(u.body, bundle, "raw bundle bytes PUT verbatim");
}

/// Drive `register_session` against a server that always returns the given
/// canned error response, and return the mapped `TransportError`.
fn error_from(status: &str, body: &'static [u8]) -> TransportError {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let base = format!("http://127.0.0.1:{port}");
    let status = status.to_string();
    let handle = serve(listener, 1, move |_req| http_response(&status, body));

    let transport = HttpTransport::new(Some(base));
    let err = transport
        .register_session("APP_TOKEN", br#"{}"#)
        .unwrap_err();
    handle.join().unwrap();
    err
}

#[test]
fn app_error_12003_maps_to_duplicate_dropped() {
    let err = error_from(
        "400 Bad Request",
        br#"{"ok":false,"result":{},"error":{"code":12003}}"#,
    );
    assert!(
        matches!(err, TransportError::DuplicateDropped),
        "got {err:?}"
    );
}

#[test]
fn app_error_12004_maps_to_too_many_similar() {
    let err = error_from(
        "400 Bad Request",
        br#"{"ok":false,"result":{},"error":{"code":12004}}"#,
    );
    assert!(
        matches!(err, TransportError::TooManySimilar { .. }),
        "got {err:?}"
    );
}

#[test]
fn http_401_maps_to_session_expired() {
    let err = error_from("401 Unauthorized", br#"{"ok":false,"error":{"code":0}}"#);
    assert!(matches!(err, TransportError::SessionExpired), "got {err:?}");
}

#[test]
fn http_503_maps_to_transient() {
    let err = error_from(
        "503 Service Unavailable",
        br#"{"ok":false,"error":{"code":0}}"#,
    );
    assert!(matches!(err, TransportError::Transient(_)), "got {err:?}");
}

// ---------------------------------------------------------------------------
// Application errors on HTTP 200.
//
// Every test above uses a 4xx/5xx status, which encodes an assumption the real
// API does not hold: it signals APPLICATION errors with an `ok:false` body and
// HTTP **200**. `ureq` only produces `Error::Status` for 4xx/5xx, so the
// envelope was never consulted on the status the server actually uses — the
// miss degraded to "no endpoint in response", the report was abandoned, and the
// server's explanation was discarded.
//
// The bodies below are the ones apidev.bugsee.com really returned.
// ---------------------------------------------------------------------------

/// Drive `create_issue` against a canned response and return the mapped error.
fn issue_error_from(status: &str, body: &'static [u8]) -> TransportError {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let base = format!("http://127.0.0.1:{port}");
    let status = status.to_string();
    let handle = serve(listener, 1, move |_req| http_response(&status, body));

    let transport = HttpTransport::new(Some(base));
    let err = transport
        .create_issue("APP_TOKEN", Some("ACCESS"), br#"{}"#)
        .unwrap_err();
    handle.join().unwrap();
    err
}

#[test]
fn validation_error_on_http_200_surfaces_the_server_message() {
    // Verbatim from apidev: a Rust report whose environment omits
    // `platform.jailbreak`, which the appserver aggregates into NaN.
    let err = issue_error_from(
        "200 OK",
        br#"{"ok":false,"error":{"type":"DataValidationError","message":"Cast to Number failed for value \"NaN\" (type number) at path \"statistics.jailbreak\"\n","code":99006}}"#,
    );
    match err {
        TransportError::Permanent(detail) => {
            assert!(detail.contains("99006"), "keeps the code: {detail}");
            assert!(
                detail.contains("statistics.jailbreak"),
                "keeps the server's own words — the only account of why the \
                 report was dropped: {detail}"
            );
            assert!(
                !detail.contains("no endpoint"),
                "must not degrade to the opaque shape-miss message: {detail}"
            );
        }
        other => panic!("expected Permanent, got {other:?}"),
    }
}

#[test]
fn app_error_12003_on_http_200_still_maps_to_duplicate_dropped() {
    // Previously reachable only on a 4xx. On a 200 this fell through to
    // Permanent, so the duplicate was never recognised as one.
    let err = issue_error_from("200 OK", br#"{"ok":false,"error":{"code":12003}}"#);
    assert!(
        matches!(err, TransportError::DuplicateDropped),
        "got {err:?}"
    );
}

#[test]
fn app_error_12004_on_http_200_still_blacklists() {
    // Same reachability bug, but this one skipped the signature blacklist, so
    // identical crashes would be re-uploaded forever.
    let err = issue_error_from("200 OK", br#"{"ok":false,"error":{"code":12004}}"#);
    assert!(
        matches!(err, TransportError::TooManySimilar { .. }),
        "got {err:?}"
    );
}

#[test]
fn ok_false_without_any_detail_is_rejected_as_an_envelope_error() {
    // Both the old and the new code call this Permanent, so asserting only the
    // variant would pass either way. What must hold is that it was rejected
    // *because the envelope said so* — not because the response happened to
    // lack the field we then went looking for.
    let err = issue_error_from("200 OK", br#"{"ok":false}"#);
    match err {
        TransportError::Permanent(detail) => assert!(
            !detail.contains("no endpoint"),
            "must be diagnosed as a server rejection, not a shape miss: {detail}"
        ),
        other => panic!("expected Permanent, got {other:?}"),
    }
}

#[test]
fn register_session_also_honours_ok_false_on_200() {
    let err = error_from(
        "200 OK",
        br#"{"ok":false,"error":{"code":99006,"message":"bad environment"}}"#,
    );
    match err {
        TransportError::Permanent(detail) => {
            assert!(detail.contains("bad environment"), "got {detail}");
            assert!(!detail.contains("no access_token"), "got {detail}");
        }
        other => panic!("expected Permanent, got {other:?}"),
    }
}

#[test]
fn a_successful_envelope_is_untouched() {
    // `ok:true` (and a body with no `ok` at all, e.g. the presigned PUT target)
    // must not be diverted into the error path.
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let base = format!("http://127.0.0.1:{port}");
    let handle = serve(listener, 1, move |_req| {
        http_response(
            "200 OK",
            br#"{"ok":true,"result":{"endpoint":"http://example.invalid/put"}}"#,
        )
    });

    let transport = HttpTransport::new(Some(base));
    let ep = transport
        .create_issue("APP_TOKEN", Some("ACCESS"), br#"{}"#)
        .expect("ok:true must pass through");
    handle.join().unwrap();
    assert_eq!(ep, "http://example.invalid/put");
}
