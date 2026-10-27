//! Exercise the actual HTTP server and startup contract, without Wine or a game.
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

const SECRET: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

struct Trainer(Child);

impl Drop for Trainer {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn request(port: u16, method: &str, path: &str, headers: &str, body: &str) -> String {
    let mut stream = TcpStream::connect(("127.0.0.1", port)).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(3)))
        .unwrap();
    write!(stream,
        "{method} {path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nConnection: close\r\n{headers}\r\n{body}"
    ).unwrap();
    let mut response = String::new();
    stream.read_to_string(&mut response).unwrap();
    response
}

#[test]
fn runtime_fails_closed_without_a_valid_secret() {
    for secret in [None, Some(""), Some("invalid")] {
        let mut command = Command::new(env!("CARGO_BIN_EXE_trainer-runtime"));
        command.env_remove("DECKY_TRAINER_SECRET");
        if let Some(secret) = secret {
            command.env("DECKY_TRAINER_SECRET", secret);
        }
        let output = command.output().unwrap();
        assert!(!output.status.success());
        assert!(String::from_utf8_lossy(&output.stderr).contains("DECKY_TRAINER_SECRET"));
        assert!(!String::from_utf8_lossy(&output.stdout).contains("listening"));
    }
}

#[cfg(unix)]
#[test]
fn invalid_environment_values_are_not_echoed_in_errors() {
    use std::ffi::OsStr;
    use std::os::unix::ffi::OsStrExt;
    let output = Command::new(env!("CARGO_BIN_EXE_trainer-runtime"))
        .env(
            "DECKY_TRAINER_SECRET",
            OsStr::from_bytes(b"private-credential-\xff"),
        )
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(!String::from_utf8_lossy(&output.stderr).contains("private-credential"));
}

#[test]
fn api_operations_require_bearer_authentication_with_cors_enabled() {
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
    let mut trainer = Trainer(child);
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if TcpStream::connect(("127.0.0.1", port)).is_ok() {
            break;
        }
        assert!(
            trainer.0.try_wait().unwrap().is_none(),
            "trainer exited at startup"
        );
        assert!(Instant::now() < deadline, "trainer did not bind its port");
        std::thread::sleep(Duration::from_millis(20));
    }

    for (method, path) in [
        ("GET", "/api/features"),
        ("GET", "/api/debug/read?expr=0&size=16"),
        ("GET", "/api/debug/symbols"),
        ("POST", "/api/features/toggle"),
        ("POST", "/api/features/set"),
        ("POST", "/api/features/activate_all"),
        ("POST", "/api/features/deactivate_all"),
        ("POST", "/api/process/attach"),
        ("POST", "/api/shutdown"),
        ("GET", "/unknown"),
    ] {
        let response = request(port, method, path, "", "");
        assert!(
            response.starts_with("HTTP/1.1 401"),
            "{method} {path}: {response}"
        );
        assert!(response.contains("www-authenticate: Bearer"));
        assert!(response.contains("access-control-allow-origin: *"));
        assert!(!response.contains(SECRET));
    }

    // A browser preflight carries no bearer token and must permit the headers
    // the authenticated request needs, without returning any trainer data.
    let response = request(
        port,
        "OPTIONS",
        "/api/features",
        "Origin: https://client.example\r\nAccess-Control-Request-Method: POST\r\nAccess-Control-Request-Headers: authorization, content-type\r\n",
        "",
    );
    assert!(response.starts_with("HTTP/1.1 204"));
    assert!(response.contains("access-control-allow-origin: *"));
    assert!(response.contains("access-control-allow-methods: GET, POST, OPTIONS"));
    assert!(response.contains("access-control-allow-headers: Content-Type, Authorization"));
    assert!(response.ends_with("\r\n\r\n"));
    for headers in [
        "Authorization: Bearer wrong\r\n".to_owned(),
        format!("Authorization: Basic {SECRET}\r\n"),
        format!("Authorization: Bearer {SECRET}\r\nAuthorization: Bearer wrong\r\n"),
        "Origin: https://attacker.example\r\nAccess-Control-Request-Method: POST\r\n".to_owned(),
    ] {
        assert!(
            request(port, "POST", "/api/features/set", &headers, "").starts_with("HTTP/1.1 401")
        );
    }
    // A token in a URL is never accepted as authentication.
    assert!(request(
        port,
        "GET",
        &format!("/api/features?token={SECRET}"),
        "",
        ""
    )
    .starts_with("HTTP/1.1 401"));
    // No body bytes arrive: unauthorized callers must be rejected immediately.
    assert!(request(
        port,
        "POST",
        "/api/features/set",
        "Content-Length: 1000000\r\n",
        ""
    )
    .starts_with("HTTP/1.1 401"));

    let auth = format!("Authorization: Bearer {SECRET}\r\nOrigin: https://client.example\r\n");
    let response = request(port, "GET", "/api/features", &auth, "");
    assert!(response.starts_with("HTTP/1.1 200"));
    assert!(response.contains("\"features\":[]"));
    assert!(response.contains("cache-control: no-store"));
    assert!(response.contains("access-control-allow-origin: *"));
    assert!(!response.contains(SECRET));

    // Authenticated calls reach the existing handlers, including their errors.
    let headers = format!("{auth}Content-Length: 2\r\nContent-Type: application/json\r\n");
    assert!(
        request(port, "POST", "/api/features/toggle", &headers, "{}").starts_with("HTTP/1.1 404")
    );
    assert!(
        request(port, "POST", "/api/features/toggle", &headers, "xx").starts_with("HTTP/1.1 400")
    );
    assert!(request(port, "POST", "/api/shutdown", &headers, "{}").starts_with("HTTP/1.1 200"));
}
