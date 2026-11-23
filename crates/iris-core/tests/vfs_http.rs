use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::net::{TcpListener, UdpSocket};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;

use iris_core::vfs::{fetch_head_bytes, validate_stream_url};

#[test]
fn validate_stream_url_enforces_schemes_loopback_ports_and_link_local_blocks() {
    for valid in [
        "https://cdn.example.com/video/ep1.mp4",
        "http://192.168.1.20/dav/movie.mkv",
        "http://127.0.0.1:1024/bridge/stream.mp4",
        "http://localhost:8080/smb/ep2.mkv",
        "http://[::1]:9000/sftp/clip.webm",
    ] {
        assert!(
            validate_stream_url(valid).is_ok(),
            "expected valid stream URL: {valid}"
        );
    }

    for invalid in [
        "",
        "file:///etc/passwd",
        "lavfi://testsrc",
        "av://lavfi:sine",
        "edl://!new_stream",
        "ftp://example.com/a.mp4",
        "https://example.com/bad\r\nurl",
        "https://example.com/bad\0url",
        "http://0.0.0.0:8080/a.mp4",
        "http://[::]:8080/a.mp4",
        "http://169.254.169.254/latest/meta-data/",
        "http://[fe80::1]:8080/a.mp4",
        "http://127.0.0.1/default_port_80.mp4",
        "http://127.0.0.1:80/admin",
        "http://localhost:22/ssh",
        "http://[::1]:1023/low_port",
    ] {
        assert!(
            validate_stream_url(invalid).is_err(),
            "expected stream URL to be rejected: {invalid}"
        );
    }
}

#[test]
fn crlf_injection_is_stripped_on_wire_and_head_bytes_are_bounded() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let captured = Arc::new(Mutex::new(String::new()));
    let captured_srv = Arc::clone(&captured);

    let srv = thread::spawn(move || {
        let (mut sock, _) = listener.accept().unwrap();
        let mut buf = [0u8; 4096];
        let n = sock.read(&mut buf).unwrap();
        *captured_srv.lock().unwrap() = String::from_utf8_lossy(&buf[..n]).into_owned();
        // Server ignores Range and sends 200 OK with 1000 bytes; client must truncate at 64.
        let body = vec![b'Z'; 1000];
        let head = format!(
            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            body.len()
        );
        let _ = sock.write_all(head.as_bytes());
        let _ = sock.write_all(&body);
    });

    let mut headers = BTreeMap::new();
    headers.insert(
        "Authorization".into(),
        "Bearer good\r\nX-Injected: evil".into(),
    );
    headers.insert("X-Bad\r\nEvil: 1".into(), "val\0ue\r\n".into());

    let head =
        fetch_head_bytes(&format!("http://127.0.0.1:{port}/photo.jpg"), &headers, 64).unwrap();
    assert_eq!(head.len(), 64);
    assert!(head.iter().all(|&b| b == b'Z'));
    srv.join().unwrap();

    let raw_req = captured.lock().unwrap().clone();
    for line in raw_req.split("\r\n") {
        assert!(
            !line.starts_with("X-Injected:") && !line.starts_with("Evil:"),
            "header injection must never create a separate header line: {raw_req}"
        );
    }
}

#[test]
fn chunked_decoding_and_credential_stripping_across_redirect() {
    let target_listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let target_port = target_listener.local_addr().unwrap().port();
    let target_req = Arc::new(Mutex::new(String::new()));
    let target_req_clone = Arc::clone(&target_req);

    let target_srv = thread::spawn(move || {
        let (mut sock, _) = target_listener.accept().unwrap();
        let mut buf = [0u8; 4096];
        let n = sock.read(&mut buf).unwrap();
        *target_req_clone.lock().unwrap() = String::from_utf8_lossy(&buf[..n]).into_owned();
        let body = concat!(
            "HTTP/1.1 206 Partial Content\r\n",
            "Transfer-Encoding: chunked\r\n",
            "Connection: close\r\n\r\n",
            "5;ext=1\r\nhello\r\n",
            "6\r\n world\r\n",
            "0\r\nX-Trailer: done\r\n\r\n"
        );
        sock.write_all(body.as_bytes()).unwrap();
    });

    let redirect_listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let redirect_port = redirect_listener.local_addr().unwrap().port();
    let redirect_srv = thread::spawn(move || {
        let (mut sock, _) = redirect_listener.accept().unwrap();
        let mut buf = [0u8; 2048];
        let _ = sock.read(&mut buf);
        let resp = format!(
            "HTTP/1.1 302 Found\r\nLocation: http://127.0.0.1:{target_port}/img.jpg\r\nContent-Length: 0\r\n\r\n"
        );
        sock.write_all(resp.as_bytes()).unwrap();
    });

    let mut headers = BTreeMap::new();
    headers.insert("Authorization".into(), "Bearer super-secret".into());
    headers.insert("Proxy-Authorization".into(), "Basic dXNlcjpwYXNz".into());
    headers.insert("Cookie".into(), "sid=12345".into());

    let payload = fetch_head_bytes(
        &format!("http://127.0.0.1:{redirect_port}/start"),
        &headers,
        64,
    )
    .unwrap();
    assert_eq!(payload, b"hello world");

    redirect_srv.join().unwrap();
    target_srv.join().unwrap();

    let forwarded = target_req.lock().unwrap().to_ascii_lowercase();
    assert!(!forwarded.contains("authorization:"));
    assert!(!forwarded.contains("proxy-authorization:"));
    assert!(!forwarded.contains("cookie:"));
    assert!(forwarded.contains("range: bytes=0-63"));
}

#[test]
fn redirect_ssrf_blocks_link_local_file_scheme_and_external_to_loopback() {
    // 1. Redirect to link-local metadata IP (169.254.169.254) is blocked before connecting.
    let redir_meta = TcpListener::bind("127.0.0.1:0").unwrap();
    let meta_port = redir_meta.local_addr().unwrap().port();
    let srv1 = thread::spawn(move || {
        let (mut sock, _) = redir_meta.accept().unwrap();
        let mut buf = [0u8; 1024];
        let _ = sock.read(&mut buf);
        sock.write_all(
            b"HTTP/1.1 302 Found\r\nLocation: http://169.254.169.254/latest/meta-data/\r\nContent-Length: 0\r\n\r\n",
        )
        .unwrap();
    });
    assert!(
        fetch_head_bytes(
            &format!("http://127.0.0.1:{meta_port}/a.jpg"),
            &BTreeMap::new(),
            32
        )
        .is_err()
    );
    srv1.join().unwrap();

    // 2. Non-loopback LAN listener redirecting to 127.0.0.1:<port> is blocked by SSRF guard.
    let lan_ip = UdpSocket::bind("0.0.0.0:0")
        .and_then(|s| {
            s.connect("192.0.2.1:80")?;
            Ok(s.local_addr()?.ip())
        })
        .ok()
        .filter(|ip| !ip.is_loopback() && !ip.is_unspecified());

    if let Some(ip) = lan_ip
        && let Ok(ext_listener) = TcpListener::bind((ip, 0))
    {
        let ext_addr = ext_listener.local_addr().unwrap();
        let victim_listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let victim_port = victim_listener.local_addr().unwrap().port();
        let victim_hit = Arc::new(AtomicBool::new(false));
        let victim_hit_srv = Arc::clone(&victim_hit);

        let _ = victim_listener.set_nonblocking(true);
        let ext_srv = thread::spawn(move || {
            let (mut sock, _) = ext_listener.accept().unwrap();
            let mut buf = [0u8; 1024];
            let _ = sock.read(&mut buf);
            let resp = format!(
                "HTTP/1.1 301 Moved Permanently\r\nLocation: http://127.0.0.1:{victim_port}/secret\r\nContent-Length: 0\r\n\r\n"
            );
            sock.write_all(resp.as_bytes()).unwrap();
        });

        let res = fetch_head_bytes(
            &format!("http://{ext_addr}/public.jpg"),
            &BTreeMap::new(),
            32,
        );
        assert!(
            res.is_err(),
            "external -> loopback redirect must fail closed"
        );
        ext_srv.join().unwrap();

        if victim_listener.accept().is_ok() {
            victim_hit_srv.store(true, Ordering::SeqCst);
        }
        assert!(
            !victim_hit.load(Ordering::SeqCst),
            "loopback victim listener must never receive redirected request"
        );
    }
}
