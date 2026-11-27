use iris_base::net::http_client::validate_stream_url;

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
