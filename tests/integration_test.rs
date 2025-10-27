use nu_plugin_test_support::PluginTest;
use nu_protocol::ShellError;
use std::io::{Read, Write};
use std::net::TcpStream;
#[cfg(unix)]
use std::os::unix::net::UnixStream;
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};
use std::thread;
use std::time::Duration;
#[cfg(windows)]
use uds_windows::UnixStream;

/// Helper to test the HTTP server using PluginTest.
///
/// Since `http serve` is a long-running command that blocks, we spawn it in a
/// background thread so we can make HTTP requests against it from the main test thread.
struct PluginTestServer {
    _server_thread: thread::JoinHandle<Result<(), ShellError>>,
    address: String,
    shutdown: Arc<AtomicBool>,
}

impl PluginTestServer {
    /// Start a test server with the given address and closure
    fn new(addr: &str, closure: &str) -> Result<Self, ShellError> {
        use nu_plugin_http_serve::HttpServePlugin;

        let mut plugin_test = PluginTest::new("http", HttpServePlugin::new().into())?;
        let address = addr.to_string();
        let cmd = format!("http serve {} {}", addr, closure);
        let shutdown = Arc::new(AtomicBool::new(false));

        // Spawn the server in a background thread
        let server_thread = thread::spawn(move || {
            // This will block until the server shuts down
            plugin_test.eval(&cmd)?;
            Ok(())
        });

        // Give the server time to start
        thread::sleep(Duration::from_millis(500));

        Ok(PluginTestServer {
            _server_thread: server_thread,
            address,
            shutdown,
        })
    }

    /// Send an HTTP request over TCP
    fn request_tcp(&self, path: &str) -> std::io::Result<String> {
        let mut stream = TcpStream::connect(&self.address)?;
        stream.set_read_timeout(Some(Duration::from_secs(2)))?;

        write!(
            stream,
            "GET {} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n",
            path
        )?;

        let mut response = String::new();
        stream.read_to_string(&mut response)?;
        Ok(response)
    }

    /// Send an HTTP request over Unix socket
    fn request_unix(&self, path: &str) -> std::io::Result<String> {
        let mut stream = UnixStream::connect(&self.address)?;
        stream.set_read_timeout(Some(Duration::from_secs(2)))?;

        write!(
            stream,
            "GET {} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n",
            path
        )?;

        let mut response = String::new();
        stream.read_to_string(&mut response)?;
        Ok(response)
    }
}

impl Drop for PluginTestServer {
    fn drop(&mut self) {
        // Signal shutdown
        self.shutdown.store(true, Ordering::SeqCst);

        // Note: The server thread will be abandoned since we can't easily
        // send Ctrl-C from here. In a real implementation, we'd need a
        // graceful shutdown mechanism.
    }
}

#[test]
fn test_tcp_basic_request() -> Result<(), ShellError> {
    let server = PluginTestServer::new("127.0.0.1:18765", r#"{|req| "Hello, World!"}"#)?;

    let response = server.request_tcp("/").expect("Failed to send request");
    assert!(response.contains("HTTP/1.1 200"));
    assert!(response.contains("Hello, World!"));
    Ok(())
}

#[test]
fn test_tcp_echo_method() -> Result<(), ShellError> {
    let server = PluginTestServer::new("127.0.0.1:18766", r#"{|req| $req.method}"#)?;

    let response = server.request_tcp("/test").expect("Failed to send request");
    assert!(response.contains("HTTP/1.1 200"));
    assert!(response.contains("GET"));
    Ok(())
}

#[test]
fn test_tcp_echo_path() -> Result<(), ShellError> {
    let server = PluginTestServer::new("127.0.0.1:18767", r#"{|req| $req.path}"#)?;

    let response = server
        .request_tcp("/test/path")
        .expect("Failed to send request");
    assert!(response.contains("HTTP/1.1 200"));
    assert!(response.contains("/test/path"));
    Ok(())
}

#[test]
fn test_unix_socket_basic_request() -> Result<(), ShellError> {
    // Use platform-appropriate temp directory
    let socket_path = std::env::temp_dir().join("nu_http_test.sock");
    let socket_path_str = socket_path.to_string_lossy().to_string();

    // Clean up any existing socket
    let _ = std::fs::remove_file(&socket_path);

    let server = PluginTestServer::new(&socket_path_str, r#"{|req| "Unix Socket Works!"}"#)?;

    let response = server.request_unix("/").expect("Failed to send request");
    assert!(response.contains("HTTP/1.1 200"));
    assert!(response.contains("Unix Socket Works!"));

    // Clean up
    let _ = std::fs::remove_file(&socket_path);
    Ok(())
}

#[test]
fn test_json_response() -> Result<(), ShellError> {
    let server = PluginTestServer::new(
        "127.0.0.1:18768",
        r#"{|req| {status: "ok", method: $req.method}}"#,
    )?;

    let response = server.request_tcp("/").expect("Failed to send request");
    assert!(response.contains("HTTP/1.1 200"));
    assert!(response.contains("application/json"));
    assert!(response.contains(r#""status""#));
    assert!(response.contains(r#""method""#));
    Ok(())
}
