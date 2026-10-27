//! Exercise body limits against the actual HTTP server, without a game or Wine.
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

const LIMIT: usize = 1024 * 1024;
const SECRET: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

struct Trainer {
    child: Child,
    port: u16,
}

impl Trainer {
    fn start() -> Self {
        let reservation = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let port = reservation.local_addr().unwrap().port();
        drop(reservation);
        let child = Command::new(env!("CARGO_BIN_EXE_trainer-runtime"))
            .args(["--port", &port.to_string()])
            .env("DECKY_TRAINER_SECRET", SECRET)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let mut trainer = Self { child, port };
        let deadline = Instant::now() + Duration::from_secs(10);
        while TcpStream::connect(("127.0.0.1", port)).is_err() {
            assert!(
                trainer.child.try_wait().unwrap().is_none(),
                "trainer exited at startup"
            );
            assert!(Instant::now() < deadline, "trainer did not bind its port");
            std::thread::sleep(Duration::from_millis(20));
        }
        trainer
    }

    fn request(&self, method: &str, path: &str, headers: &str, body: &[u8]) -> String {
        let mut stream = TcpStream::connect(("127.0.0.1", self.port)).unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(3)))
            .unwrap();
        stream
            .set_write_timeout(Some(Duration::from_secs(3)))
            .unwrap();
        write!(
            stream,
            "{method} {path} HTTP/1.1\r\nHost: 127.0.0.1:{}\r\nConnection: close\r\n{headers}\r\n",
            self.port
        )
        .unwrap();
        // A rejection may close the connection before the caller finishes
        // transmitting. Preserve the response in that case.
        if let Err(error) = stream.write_all(body) {
            assert!(
                matches!(
                    error.kind(),
                    std::io::ErrorKind::BrokenPipe | std::io::ErrorKind::ConnectionReset
                ),
                "{error}"
            );
        }
        let mut response = Vec::new();
        if let Err(error) = stream.read_to_end(&mut response) {
            assert!(
                error.kind() == std::io::ErrorKind::ConnectionReset && !response.is_empty(),
                "{error}"
            );
        }
        String::from_utf8(response).unwrap()
    }

    fn fixed(&self, method: &str, path: &str, body: &[u8]) -> String {
        self.request(method, path, &format!("Authorization: Bearer {SECRET}\r\nContent-Length: {}\r\nContent-Type: application/json\r\n", body.len()), body)
    }

    fn chunked(&self, method: &str, path: &str, body: &[u8]) -> String {
        let mut encoded = Vec::new();
        for chunk in body.chunks(16 * 1024) {
            write!(encoded, "{:x}\r\n", chunk.len()).unwrap();
            encoded.extend_from_slice(chunk);
            encoded.extend_from_slice(b"\r\n");
        }
        encoded.extend_from_slice(b"0\r\n\r\n");
        self.request(method, path, &format!("Authorization: Bearer {SECRET}\r\nTransfer-Encoding: chunked\r\nContent-Type: application/json\r\n"), &encoded)
    }
}

impl Drop for Trainer {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn json_body(size: usize) -> Vec<u8> {
    let mut body = vec![b' '; size];
    body[..2].copy_from_slice(b"{}");
    body
}

fn assert_json_status(response: &str, status: u16) {
    assert!(
        response.starts_with(&format!("HTTP/1.1 {status} ")),
        "unexpected response: {response}"
    );
    assert!(
        response.contains("access-control-allow-origin: *"),
        "CORS missing: {response}"
    );
    assert!(
        !response.contains(SECRET),
        "response leaked authentication secret"
    );
    let (_, body) = response.split_once("\r\n\r\n").unwrap();
    let payload: serde_json::Value =
        serde_json::from_str(body).expect("response must contain JSON");
    if status >= 400 {
        assert!(payload["error"].is_string(), "missing error: {response}");
    }
}

#[test]
fn fixed_length_and_chunked_bodies_allow_exactly_one_mib() {
    let trainer = Trainer::start();
    let body = json_body(LIMIT);
    // Empty trainers have no matching record, so 404 proves valid JSON was
    // parsed and dispatched rather than rejected by the size guard.
    assert_json_status(&trainer.fixed("POST", "/api/features/toggle", &body), 404);
    assert_json_status(&trainer.chunked("POST", "/api/features/toggle", &body), 404);
    let body = json_body(LIMIT + 1);
    assert_json_status(&trainer.fixed("POST", "/api/features/toggle", &body), 413);
    assert_json_status(&trainer.chunked("POST", "/api/features/toggle", &body), 413);
    assert_json_status(&trainer.fixed("POST", "/api/features/toggle", b"xx"), 400);
    assert_json_status(&trainer.fixed("POST", "/api/features/set", b"xx"), 400);
}

#[test]
fn declared_oversize_is_rejected_without_waiting_for_body_bytes() {
    let trainer = Trainer::start();
    let headers = format!(
        "Authorization: Bearer {SECRET}\r\nContent-Length: {}\r\n",
        LIMIT + 1
    );
    // Keep the write side open. The socket timeout fails this test if the
    // server waits for the advertised body instead of rejecting its length.
    assert_json_status(
        &trainer.request("POST", "/api/features/set", &headers, b""),
        413,
    );

    let headers = format!("Content-Length: {}\r\n", LIMIT + 1);
    let response = trainer.request("POST", "/api/features/set", &headers, b"");
    assert_json_status(&response, 401);
    assert!(response.contains("www-authenticate: Bearer"));

    let response = trainer.request("OPTIONS", "/api/features/set", &headers, b"");
    assert!(response.starts_with("HTTP/1.1 204 "), "{response}");
    assert!(response.contains("access-control-allow-origin: *"));
    assert!(response.contains("access-control-allow-headers: Content-Type, Authorization"));
    assert!(response.ends_with("\r\n\r\n"));
}

#[test]
fn oversized_bodies_cannot_reach_routes_that_normally_ignore_the_body() {
    let trainer = Trainer::start();
    let body = json_body(LIMIT + 1);
    for (method, path) in [
        ("POST", "/api/shutdown"),
        ("POST", "/api/features/activate_all"),
        ("POST", "/api/features/deactivate_all"),
        ("GET", "/api/features"),
    ] {
        assert_json_status(&trainer.fixed(method, path, &body), 413);
        assert_json_status(&trainer.chunked(method, path, &body), 413);
        // In particular, an oversized shutdown must leave the server alive.
        assert_json_status(&trainer.fixed("GET", "/api/features", b""), 200);
    }
}

#[test]
fn malformed_chunked_bodies_return_400_before_dispatch() {
    let trainer = Trainer::start();
    let headers = format!("Authorization: Bearer {SECRET}\r\nTransfer-Encoding: chunked\r\n");
    for path in ["/api/features/toggle", "/api/shutdown"] {
        // An invalid chunk size exercises body collection errors rather than
        // the JSON parser, and must not be mistaken for a limit violation.
        assert_json_status(&trainer.request("POST", path, &headers, b"z\r\n"), 400);
        assert_json_status(&trainer.fixed("GET", "/api/features", b""), 200);
    }
}
