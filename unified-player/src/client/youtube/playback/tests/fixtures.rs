fn format(url: Option<&str>, mime_type: &str, bitrate: u64) -> AdaptiveFormat {
    AdaptiveFormat {
        itag: 140,
        url: url.map(ToString::to_string),
        signature_cipher: url.is_none().then(|| "redacted-cipher".to_string()),
        cipher: None,
        mime_type: mime_type.to_string(),
        bitrate,
        content_length: Some("1024".to_string()),
        approx_duration_ms: Some("5000".to_string()),
    }
}

#[cfg(feature = "private-capture")]
fn serve_one_player_response(
    status: u16,
    response_body: &'static [u8],
) -> (
    reqwest::Url,
    std::sync::mpsc::Receiver<Vec<u8>>,
    std::thread::JoinHandle<()>,
) {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let (sender, receiver) = std::sync::mpsc::channel();
    let thread = std::thread::spawn(move || {
        let (mut socket, _) = listener.accept().unwrap();
        socket
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        let mut request = Vec::new();
        let mut buffer = [0_u8; 4096];
        let header_end = loop {
            let read = socket.read(&mut buffer).unwrap();
            assert!(read > 0);
            request.extend_from_slice(&buffer[..read]);
            if let Some(index) = request.windows(4).position(|window| window == b"\r\n\r\n") {
                break index + 4;
            }
        };
        let headers = std::str::from_utf8(&request[..header_end]).unwrap();
        let content_length = headers
            .lines()
            .find_map(|line| {
                let (name, value) = line.split_once(':')?;
                name.eq_ignore_ascii_case("content-length")
                    .then(|| value.trim().parse::<usize>().unwrap())
            })
            .unwrap_or_default();
        while request.len() < header_end + content_length {
            let read = socket.read(&mut buffer).unwrap();
            assert!(read > 0);
            request.extend_from_slice(&buffer[..read]);
        }
        let _ = sender.send(request[header_end..header_end + content_length].to_vec());
        let reason = match status {
            200 => "OK",
            403 => "Forbidden",
            429 => "Too Many Requests",
            _ => "Fixture",
        };
        write!(
            socket,
            "HTTP/1.1 {status} {reason}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            response_body.len()
        )
        .unwrap();
        socket.write_all(response_body).unwrap();
        socket.flush().unwrap();
    });
    (
        reqwest::Url::parse(&format!("http://{address}/youtubei/v1/player?key=fixture")).unwrap(),
        receiver,
        thread,
    )
}

#[cfg(feature = "private-capture")]
fn serve_player_response_sequence(
    responses: Vec<(u16, &'static [u8])>,
) -> (
    reqwest::Url,
    std::sync::mpsc::Receiver<Vec<u8>>,
    std::thread::JoinHandle<()>,
) {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let (sender, receiver) = std::sync::mpsc::channel();
    let thread = std::thread::spawn(move || {
        for (status, response_body) in responses {
            let (mut socket, _) = listener.accept().unwrap();
            let request = read_fixture_request_payload(&mut socket);
            sender.send(request).unwrap();
            let reason = match status {
                200 => "OK",
                403 => "Forbidden",
                429 => "Too Many Requests",
                _ => "Fixture",
            };
            write!(
                socket,
                "HTTP/1.1 {status} {reason}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                response_body.len()
            )
            .unwrap();
            socket.write_all(response_body).unwrap();
            socket.flush().unwrap();
        }
    });
    (
        reqwest::Url::parse(&format!("http://{address}/youtubei/v1/player?key=fixture")).unwrap(),
        receiver,
        thread,
    )
}

#[cfg(feature = "private-capture")]
fn serve_counted_replay_response(
    status: u16,
    response_body: &'static [u8],
    delay: Duration,
) -> (
    reqwest::Url,
    std::sync::mpsc::Receiver<Vec<u8>>,
    std::sync::Arc<std::sync::atomic::AtomicUsize>,
    std::thread::JoinHandle<()>,
) {
    use std::sync::atomic::Ordering;

    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let (sender, receiver) = std::sync::mpsc::channel();
    let requests = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let thread_requests = requests.clone();
    let thread = std::thread::spawn(move || {
        let (mut socket, _) = listener.accept().unwrap();
        thread_requests.fetch_add(1, Ordering::SeqCst);
        socket
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        let mut request = Vec::new();
        let mut buffer = [0_u8; 4096];
        let header_end = loop {
            let read = socket.read(&mut buffer).unwrap();
            assert!(read > 0);
            request.extend_from_slice(&buffer[..read]);
            if let Some(index) = request.windows(4).position(|window| window == b"\r\n\r\n") {
                break index + 4;
            }
        };
        let headers = std::str::from_utf8(&request[..header_end]).unwrap();
        let content_length = headers
            .lines()
            .find_map(|line| {
                let (name, value) = line.split_once(':')?;
                name.eq_ignore_ascii_case("content-length")
                    .then(|| value.trim().parse::<usize>().unwrap())
            })
            .unwrap_or_default();
        while request.len() < header_end + content_length {
            let read = socket.read(&mut buffer).unwrap();
            assert!(read > 0);
            request.extend_from_slice(&buffer[..read]);
        }
        sender.send(request).unwrap();
        std::thread::sleep(delay);
        let _ = write!(
            socket,
            "HTTP/1.1 {status} Fixture\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            response_body.len()
        );
        let _ = socket.write_all(response_body);
        let _ = socket.flush();

        listener.set_nonblocking(true).unwrap();
        let deadline = std::time::Instant::now() + Duration::from_millis(100);
        while std::time::Instant::now() < deadline {
            match listener.accept() {
                Ok(_) => {
                    thread_requests.fetch_add(1, Ordering::SeqCst);
                }
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    std::thread::sleep(Duration::from_millis(5));
                }
                Err(_) => break,
            }
        }
    });
    (
        reqwest::Url::parse(&format!("http://{address}/youtubei/v1/player?key=fixture")).unwrap(),
        receiver,
        requests,
        thread,
    )
}

#[cfg(feature = "private-capture")]
fn serve_replay_redirect() -> (
    reqwest::Url,
    std::sync::Arc<std::sync::atomic::AtomicUsize>,
    std::sync::Arc<std::sync::atomic::AtomicUsize>,
    std::thread::JoinHandle<()>,
) {
    use std::sync::atomic::Ordering;

    let first = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let second = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let first_address = first.local_addr().unwrap();
    let second_address = second.local_addr().unwrap();
    let first_requests = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let second_requests = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let thread_first = first_requests.clone();
    let thread_second = second_requests.clone();
    let thread = std::thread::spawn(move || {
        let (mut socket, _) = first.accept().unwrap();
        thread_first.fetch_add(1, Ordering::SeqCst);
        read_fixture_request(&mut socket);
        write!(
            socket,
            "HTTP/1.1 307 Temporary Redirect\r\nLocation: http://{second_address}/youtubei/v1/player?key=redirected\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
        )
        .unwrap();
        socket.flush().unwrap();

        second.set_nonblocking(true).unwrap();
        let deadline = std::time::Instant::now() + Duration::from_millis(200);
        while std::time::Instant::now() < deadline {
            match second.accept() {
                Ok(_) => {
                    thread_second.fetch_add(1, Ordering::SeqCst);
                    break;
                }
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    std::thread::sleep(Duration::from_millis(5));
                }
                Err(_) => break,
            }
        }
    });
    (
        reqwest::Url::parse(&format!(
            "http://{first_address}/youtubei/v1/player?key=fixture"
        ))
        .unwrap(),
        first_requests,
        second_requests,
        thread,
    )
}

fn fixture_resolver(
    endpoint: reqwest::Url,
    auth_type: crate::config::YouTubeMusicAuthType,
) -> InnertubeAudioResolver {
    InnertubeAudioResolver::new_for_test(endpoint, auth_type)
}

#[cfg(feature = "private-capture")]
fn serve_delayed_player_body(
    response_body: &'static [u8],
) -> (
    reqwest::Url,
    std::sync::mpsc::Receiver<()>,
    std::thread::JoinHandle<()>,
) {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let (head_sender, head_receiver) = std::sync::mpsc::channel();
    let thread = std::thread::spawn(move || {
        let (mut socket, _) = listener.accept().unwrap();
        socket
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        let mut request = Vec::new();
        let mut buffer = [0_u8; 4096];
        while !request.windows(4).any(|window| window == b"\r\n\r\n") {
            let read = socket.read(&mut buffer).unwrap();
            assert!(read > 0);
            request.extend_from_slice(&buffer[..read]);
        }
        write!(
            socket,
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            response_body.len()
        )
        .unwrap();
        socket.flush().unwrap();
        head_sender.send(()).unwrap();
        std::thread::sleep(Duration::from_millis(500));
        let _ = socket.write_all(response_body);
        let _ = socket.flush();
    });
    (
        reqwest::Url::parse(&format!("http://{address}/youtubei/v1/player?key=fixture")).unwrap(),
        head_receiver,
        thread,
    )
}

#[cfg(feature = "private-capture")]
fn serve_blocked_player_body(
    response_body: &'static [u8],
) -> (
    reqwest::Url,
    std::sync::mpsc::Sender<()>,
    std::thread::JoinHandle<()>,
) {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let (body_release_sender, body_release_receiver) = std::sync::mpsc::channel();
    let thread = std::thread::spawn(move || {
        let (mut socket, _) = listener.accept().unwrap();
        socket
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        let mut request = Vec::new();
        let mut buffer = [0_u8; 4096];
        while !request.windows(4).any(|window| window == b"\r\n\r\n") {
            let read = socket.read(&mut buffer).unwrap();
            assert!(read > 0);
            request.extend_from_slice(&buffer[..read]);
        }
        write!(
            socket,
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            response_body.len()
        )
        .unwrap();
        socket.flush().unwrap();
        if body_release_receiver
            .recv_timeout(Duration::from_secs(5))
            .is_ok()
        {
            let _ = socket.write_all(response_body);
            let _ = socket.flush();
        }
    });
    (
        reqwest::Url::parse(&format!("http://{address}/youtubei/v1/player?key=fixture")).unwrap(),
        body_release_sender,
        thread,
    )
}

#[cfg(feature = "private-capture")]
fn serve_closed_player_connection() -> (reqwest::Url, std::thread::JoinHandle<()>) {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let thread = std::thread::spawn(move || {
        let (mut socket, _) = listener.accept().unwrap();
        let mut buffer = [0_u8; 4096];
        let _ = socket.read(&mut buffer);
    });
    (
        reqwest::Url::parse(&format!("http://{address}/youtubei/v1/player?key=fixture")).unwrap(),
        thread,
    )
}

#[cfg(feature = "private-capture")]
fn serve_media_range_responses(count: usize) -> (reqwest::Url, std::thread::JoinHandle<()>) {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let thread = std::thread::spawn(move || {
        for _ in 0..count {
            let (mut socket, _) = listener.accept().unwrap();
            read_fixture_request(&mut socket);
            let body = b"fixture-media-bytes";
            write!(
                socket,
                "HTTP/1.1 206 Partial Content\r\nContent-Type: audio/mp4\r\nContent-Range: bytes 0-{}/{}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                body.len() - 1,
                body.len(),
                body.len()
            )
            .unwrap();
            socket.write_all(body).unwrap();
            socket.flush().unwrap();
        }
    });
    (
        reqwest::Url::parse(&format!(
            "http://{address}/videoplayback?sig=fixture-private-signature"
        ))
        .unwrap(),
        thread,
    )
}

#[cfg(feature = "private-capture")]
fn serve_one_media_probe(
    status: u16,
    content_range: Option<&'static str>,
    delay: Duration,
) -> (reqwest::Url, std::thread::JoinHandle<()>) {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let thread = std::thread::spawn(move || {
        let (mut socket, _) = listener.accept().unwrap();
        read_fixture_request(&mut socket);
        if !delay.is_zero() {
            std::thread::sleep(delay);
        }
        let reason = match status {
            200 => "OK",
            206 => "Partial Content",
            403 => "Forbidden",
            _ => "Fixture",
        };
        write!(
            socket,
            "HTTP/1.1 {status} {reason}\r\nContent-Length: 0\r\n"
        )
        .unwrap();
        if let Some(content_range) = content_range {
            write!(socket, "Content-Range: {content_range}\r\n").unwrap();
        }
        write!(socket, "Connection: close\r\n\r\n").unwrap();
        socket.flush().unwrap();
    });
    (
        reqwest::Url::parse(&format!("http://{address}/videoplayback?sig=fixture")).unwrap(),
        thread,
    )
}

#[cfg(feature = "private-capture")]
fn serve_media_probe_requiring_user_agent(
    expected_user_agent: &'static str,
) -> (
    reqwest::Url,
    std::sync::mpsc::Receiver<bool>,
    std::thread::JoinHandle<()>,
) {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let (sender, receiver) = std::sync::mpsc::channel();
    let thread = std::thread::spawn(move || {
        let (mut socket, _) = listener.accept().unwrap();
        socket
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        let mut request = Vec::new();
        let mut buffer = [0_u8; 4096];
        let header_end = loop {
            let read = socket.read(&mut buffer).unwrap();
            assert!(read > 0);
            request.extend_from_slice(&buffer[..read]);
            if let Some(index) = request.windows(4).position(|window| window == b"\r\n\r\n") {
                break index + 4;
            }
        };
        let headers = std::str::from_utf8(&request[..header_end]).unwrap();
        let user_agent_matches = headers.lines().any(|line| {
            line.split_once(':').is_some_and(|(name, value)| {
                name.eq_ignore_ascii_case("user-agent") && value.trim() == expected_user_agent
            })
        });
        sender.send(user_agent_matches).unwrap();
        if user_agent_matches {
            write!(
                socket,
                "HTTP/1.1 206 Partial Content\r\nContent-Range: bytes 512-1023/1024\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
            )
            .unwrap();
        } else {
            write!(
                socket,
                "HTTP/1.1 200 OK\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
            )
            .unwrap();
        }
        socket.flush().unwrap();
    });
    (
        reqwest::Url::parse(&format!("http://{address}/videoplayback?sig=fixture")).unwrap(),
        receiver,
        thread,
    )
}

#[cfg(feature = "private-capture")]
fn media_probe_source(url: reqwest::Url) -> super::ResolvedAudioSource {
    super::ResolvedAudioSource {
        media_id: "fixture-private-id".to_owned(),
        itag: 140,
        url,
        required_headers: reqwest::header::HeaderMap::new(),
        mime_type: "audio/mp4".to_owned(),
        bitrate: 128_000,
        content_length: Some(1024),
        duration: Some(Duration::from_secs(1)),
        expires_at_unix: None,
        source_client: "fixture",
    }
}

#[cfg(feature = "private-capture")]
fn serve_redirected_media_probe() -> (reqwest::Url, std::thread::JoinHandle<()>) {
    let first = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let second = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let first_address = first.local_addr().unwrap();
    let second_address = second.local_addr().unwrap();
    let thread = std::thread::spawn(move || {
        let (mut first_socket, _) = first.accept().unwrap();
        read_fixture_request(&mut first_socket);
        write!(
            first_socket,
            "HTTP/1.1 307 Temporary Redirect\r\nLocation: http://{second_address}/videoplayback?sig=redirected\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
        )
        .unwrap();
        first_socket.flush().unwrap();
        let (mut second_socket, _) = second.accept().unwrap();
        read_fixture_request(&mut second_socket);
        write!(
            second_socket,
            "HTTP/1.1 206 Partial Content\r\nContent-Range: bytes 512-1023/1024\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
        )
        .unwrap();
        second_socket.flush().unwrap();
    });
    (
        reqwest::Url::parse(&format!("http://{first_address}/videoplayback?sig=fixture")).unwrap(),
        thread,
    )
}

#[cfg(feature = "private-capture")]
fn serve_embedded_redirect_media_probe() -> (reqwest::Url, std::thread::JoinHandle<()>) {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let thread = std::thread::spawn(move || {
        let (mut first_socket, _) = listener.accept().unwrap();
        read_fixture_request(&mut first_socket);
        let body = format!(
            "fixture-prefix\0http://{address}/videoplayback?sig=embedded-fixture\0"
        );
        write!(
            first_socket,
            "HTTP/1.1 200 OK\r\nContent-Type: application/octet-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            body.len()
        )
        .unwrap();
        first_socket.write_all(body.as_bytes()).unwrap();
        first_socket.flush().unwrap();

        listener.set_nonblocking(true).unwrap();
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        let (mut second_socket, _) = loop {
            match listener.accept() {
                Ok(connection) => break connection,
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    if std::time::Instant::now() >= deadline {
                        return;
                    }
                    std::thread::sleep(Duration::from_millis(5));
                }
                Err(_) => return,
            }
        };
        read_fixture_request(&mut second_socket);
        write!(
            second_socket,
            "HTTP/1.1 206 Partial Content\r\nContent-Range: bytes 512-1023/1024\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
        )
        .unwrap();
        second_socket.flush().unwrap();
    });
    (
        reqwest::Url::parse(&format!("http://{address}/videoplayback?sig=fixture")).unwrap(),
        thread,
    )
}

#[cfg(feature = "private-capture")]
fn read_fixture_request(socket: &mut std::net::TcpStream) {
    let _ = read_fixture_request_payload(socket);
}

#[cfg(feature = "private-capture")]
fn read_fixture_request_payload(socket: &mut std::net::TcpStream) -> Vec<u8> {
    socket
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    let mut request = Vec::new();
    let mut buffer = [0_u8; 4096];
    let header_end = loop {
        let read = socket.read(&mut buffer).unwrap();
        assert!(read > 0);
        request.extend_from_slice(&buffer[..read]);
        if let Some(index) = request.windows(4).position(|window| window == b"\r\n\r\n") {
            break index + 4;
        }
    };
    let headers = std::str::from_utf8(&request[..header_end]).unwrap();
    let content_length = headers
        .lines()
        .find_map(|line| {
            let (name, value) = line.split_once(':')?;
            name.eq_ignore_ascii_case("content-length")
                .then(|| value.trim().parse::<usize>().unwrap())
        })
        .unwrap_or_default();
    while request.len() < header_end + content_length {
        let read = socket.read(&mut buffer).unwrap();
        assert!(read > 0);
        request.extend_from_slice(&buffer[..read]);
    }
    request[header_end..header_end + content_length].to_vec()
}

#[cfg(feature = "private-capture")]
fn serve_redirected_player_response(
    response_body: &'static [u8],
) -> (reqwest::Url, std::thread::JoinHandle<()>) {
    let first = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let second = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let first_address = first.local_addr().unwrap();
    let second_address = second.local_addr().unwrap();
    let thread = std::thread::spawn(move || {
        let (mut first_socket, _) = first.accept().unwrap();
        read_fixture_request(&mut first_socket);
        write!(
            first_socket,
            "HTTP/1.1 307 Temporary Redirect\r\nLocation: http://{second_address}/youtubei/v1/player?key=redirected\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
        )
        .unwrap();
        first_socket.flush().unwrap();

        let (mut second_socket, _) = second.accept().unwrap();
        read_fixture_request(&mut second_socket);
        write!(
            second_socket,
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            response_body.len()
        )
        .unwrap();
        second_socket.write_all(response_body).unwrap();
        second_socket.flush().unwrap();
    });
    (
        reqwest::Url::parse(&format!(
            "http://{first_address}/youtubei/v1/player?key=fixture"
        ))
        .unwrap(),
        thread,
    )
}
