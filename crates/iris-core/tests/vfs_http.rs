use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::net::TcpListener;
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
        "http://localhost.:80/audio",
        "http://[::127.0.0.1]:9000/audio",
        "http://[64:ff9b::7f00:1]:9000/audio",
        "http://[2002:7f00:1::]:9000/audio",
        "http://[fec0::1]:9000/audio",
        "http://[fd00:ec2::254]:9000/audio",
        "http://192.0.0.42/audio",
        "http://198.18.0.1/audio",
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
fn informational_headers_and_chunked_decoding_survive_same_origin_redirect() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let server = thread::spawn(move || {
        let (mut first, _) = listener.accept().unwrap();
        let _ = first.read(&mut [0; 2048]).unwrap();
        first
            .write_all(b"HTTP/1.1 302 Found\r\nLocation: /audio\r\nContent-Length: 0\r\n\r\n")
            .unwrap();
        let (mut final_response, _) = listener.accept().unwrap();
        let mut buf = [0; 2048];
        let n = final_response.read(&mut buf).unwrap();
        let forwarded = std::str::from_utf8(&buf[..n]).unwrap();
        assert!(forwarded.contains("Authorization: Bearer fixture\r\n"));
        assert!(forwarded.contains("Range: bytes=0-63\r\n"));
        final_response.write_all(concat!(
            "HTTP/1.1 103 Early Hints\r\nLink: </audio>; rel=preload\r\n\r\n",
            "HTTP/1.1 100 Continue\r\n\r\n",
            "HTTP/1.1 206 Partial Content\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n",
            "5;ext=1\r\nhello\r\n6\r\n world\r\n0\r\nX-Trailer: done\r\n\r\n",
        ).as_bytes()).unwrap();
    });
    let headers = BTreeMap::from([("Authorization".into(), "Bearer fixture".into())]);
    assert_eq!(
        fetch_head_bytes(&format!("http://{addr}/start"), &headers, 64).unwrap(),
        b"hello world"
    );
    server.join().unwrap();
}

#[test]
fn redirects_revalidate_schemes_and_private_hosts_before_connecting() {
    let second = TcpListener::bind("127.0.0.1:0").unwrap();
    let other_host = format!(
        "http://localhost:{}/other",
        second.local_addr().unwrap().port()
    );
    let other_port = format!("http://{}/other", second.local_addr().unwrap());
    second.set_nonblocking(true).unwrap();
    for location in ["ftp://localhost/unsupported", &other_host, &other_port] {
        let origin = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}/media", origin.local_addr().unwrap());
        thread::scope(|scope| {
            scope.spawn(|| {
                let (mut socket, _) = origin.accept().unwrap();
                let _ = socket.read(&mut [0; 2048]);
                write!(
                    socket,
                    "HTTP/1.1 302 Found\r\nLocation: {location}\r\nContent-Length: 0\r\n\r\n"
                )
                .unwrap();
            });
            assert!(fetch_head_bytes(&url, &BTreeMap::new(), 32).is_err());
        });
        assert!(matches!(second.accept(), Err(e) if e.kind() == std::io::ErrorKind::WouldBlock));
    }
}

#[test]
fn malformed_chunk_and_conflicting_headers_fail_closed() {
    for response in [
        &b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n4\r\ndata"[..],
        &b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n4\r\ndata\r\n0\r\n"[..],
        &b"HTTP/1.1 200 OK\r\nContent-Length: 4\r\nContent-Length: 3\r\n\r\ndata"[..],
    ] {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        thread::scope(|scope| {
            scope.spawn(move || {
                let (mut socket, _) = listener.accept().unwrap();
                let _ = socket.read(&mut [0; 2048]).unwrap();
                socket.write_all(response).unwrap();
            });
            assert!(
                fetch_head_bytes(&format!("http://{addr}/audio"), &BTreeMap::new(), 64).is_err()
            );
        });
    }
}
