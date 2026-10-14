//! The real blocking transport against loopback servers: the policy behaviours (redirects refused,
//! replies capped, timeouts) and the headers a request carries.
#![cfg(not(target_arch = "wasm32"))]
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::indexing_slicing)]

use std::sync::{Arc, Mutex};
use std::time::Duration;

use craft_wsd::http::HttpTransport;
use craft_wsd::testing::{Server, raw_response};
use craft_wsd::{DeviceUrl, Error, Request, Transport};

fn url(server: &Server) -> DeviceUrl {
    DeviceUrl::parse(&server.url("/ws")).unwrap()
}

fn request() -> Request {
    Request {
        content_type: "application/soap+xml".into(),
        soap_action: "urn:example/Action".into(),
        body: b"<x/>".to_vec(),
        timeout: Duration::from_secs(5),
        max_response: 64 << 10,
    }
}

#[test]
fn a_reply_comes_back_with_status_type_and_body() {
    let server = Server::start(|_| raw_response(200, "application/soap+xml", b"<ok/>"));
    let r = HttpTransport::new().post(&url(&server), request()).unwrap();
    assert_eq!((r.status, r.content_type.as_deref(), r.body.as_slice()), (200, Some("application/soap+xml"), &b"<ok/>"[..]));
    // A non-success status is data, not an error: the caller decides (it may carry a SOAP fault).
    let busy = Server::start(|_| raw_response(503, "text/plain", b"busy"));
    assert_eq!(HttpTransport::new().post(&url(&busy), request()).unwrap().status, 503);
}

#[test]
fn redirects_are_refused_not_followed() {
    let target = Server::start(|_| raw_response(200, "text/plain", b"you should never get here"));
    let target_url = target.url("/elsewhere");
    let redirecting = Server::start(move |_| {
        format!("HTTP/1.1 302 Found\r\nLocation: {target_url}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").into_bytes()
    });
    let e = HttpTransport::new().post(&url(&redirecting), request()).unwrap_err();
    assert!(matches!(e, Error::Transport(ref m) if m.contains("redirect")), "{e:?}");
    assert_eq!(*target.hits.lock().unwrap(), 0, "the redirect target was never contacted");
}

#[test]
fn oversized_replies_are_cut_off() {
    let big = Server::start(|_| raw_response(200, "application/soap+xml", &vec![b'<'; 3 << 20]));
    let e = HttpTransport::new().post(&url(&big), request()).unwrap_err();
    assert!(matches!(e, Error::TooLarge(..)), "{e:?}");
}

#[test]
fn a_silent_device_times_out() {
    let silent = Server::start(|_| {
        std::thread::sleep(Duration::from_millis(1500));
        Vec::new()
    });
    let started = std::time::Instant::now();
    let e = HttpTransport::new().post(&url(&silent), Request { timeout: Duration::from_millis(300), ..request() }).unwrap_err();
    assert!(matches!(e, Error::Transport(_)), "{e:?}");
    assert!(started.elapsed() < Duration::from_millis(1400), "{:?}", started.elapsed());
}

#[test]
fn requests_carry_the_soap_headers() {
    let seen = Arc::new(Mutex::new(Vec::<(String, String, String, Vec<u8>)>::new()));
    let s = seen.clone();
    let server = Server::start(move |req| {
        s.lock().unwrap().push((
            req.header("soapaction").unwrap_or("").to_owned(),
            req.header("content-type").unwrap_or("").to_owned(),
            req.header("host").unwrap_or("").to_owned(),
            req.body.clone(),
        ));
        raw_response(200, "text/plain", b"")
    });
    HttpTransport::new().post(&url(&server), request()).unwrap();
    let seen = seen.lock().unwrap();
    assert_eq!(seen[0].0, "\"urn:example/Action\"");
    assert_eq!(seen[0].1, "application/soap+xml");
    assert!(seen[0].2.starts_with("127.0.0.1:"));
    assert_eq!(seen[0].3, b"<x/>");
}

#[test]
fn a_server_that_closes_without_replying_fails_fast() {
    let server = Server::start(|_| Vec::new());
    let mut r = request();
    r.timeout = Duration::from_secs(20);
    let started = std::time::Instant::now();
    let e = HttpTransport::new().post(&url(&server), r).unwrap_err();
    assert!(started.elapsed() < Duration::from_secs(5), "took {:?}: {e:?}", started.elapsed());
    assert_eq!(e, Error::ConnectionClosed);
}

/// One connection, kept alive: the first request is answered, the second is read and the socket
/// closed without a reply (what an embedded server does with a request it dislikes).
#[test]
fn a_reused_connection_closed_without_a_reply_fails_fast() {
    use std::io::{Read, Write};
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let handle = std::thread::spawn(move || {
        let (mut s, _) = listener.accept().unwrap();
        let mut buf = [0u8; 4096];
        let _ = s.read(&mut buf).unwrap();
        s.write_all(b"HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nContent-Length: 2\r\n\r\nok").unwrap();
        let _ = s.read(&mut buf).unwrap();
        drop(s);
        // Anything that arrives on a fresh connection is also dropped.
        for s in listener.incoming().take(3).flatten() {
            let mut b = [0u8; 4096];
            let mut s = s;
            let _ = s.read(&mut b);
        }
    });
    let u = DeviceUrl::parse(&format!("http://127.0.0.1:{port}/ws")).unwrap();
    let t = HttpTransport::new();
    let mut r = request();
    r.timeout = Duration::from_secs(20);
    assert_eq!(t.post(&u, r.clone()).unwrap().status, 200);
    let started = std::time::Instant::now();
    let e = t.post(&u, r).unwrap_err();
    assert!(started.elapsed() < Duration::from_secs(5), "took {:?}: {e:?}", started.elapsed());
    assert_eq!(e, Error::ConnectionClosed);
    drop(handle);
}

/// Run one request against a loopback server that misbehaves as `serve` says; the error and how long it took.
fn behaviour(serve: impl Fn(std::net::TcpStream) + Send + 'static) -> (std::time::Duration, Error) {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    std::thread::spawn(move || {
        for s in listener.incoming().take(4).flatten() {
            serve(s);
        }
    });
    let u = DeviceUrl::parse(&format!("http://127.0.0.1:{port}/ws")).unwrap();
    let mut r = request();
    r.timeout = Duration::from_secs(8);
    let started = std::time::Instant::now();
    let e = HttpTransport::new().post(&u, r).unwrap_err();
    (started.elapsed(), e)
}

#[test]
fn a_peer_that_closes_early_or_half_closes_fails_fast_as_connection_closed() {
    use std::io::Read;
    use std::net::Shutdown;
    // Reads only part of the request, then closes (a reset on most stacks).
    let (took, e) = behaviour(|mut s| {
        let mut b = [0u8; 20];
        let _ = s.read(&mut b);
        drop(s);
    });
    assert!(took < Duration::from_secs(4) && e == Error::ConnectionClosed, "{took:?} {e:?}");
    // Reads the request, shuts down its sending side and keeps the socket open: still an immediate failure.
    let (took, e) = behaviour(|mut s| {
        let mut b = [0u8; 4096];
        let _ = s.read(&mut b);
        let _ = s.shutdown(Shutdown::Write);
        std::thread::sleep(Duration::from_secs(10));
    });
    assert!(took < Duration::from_secs(4) && e == Error::ConnectionClosed, "{took:?} {e:?}");
}
