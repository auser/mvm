use anyhow::{Context, Result};
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

/// A minimal HTTP server that serves `GET /metrics` in a background thread.
///
/// Binds to `127.0.0.1:<port>` and returns the Prometheus exposition format
/// from the global metrics registry on every request. No external dependencies —
/// uses only `std::net::TcpListener`.
pub struct MetricsServer {
    shutdown: Arc<AtomicBool>,
    handle: Option<std::thread::JoinHandle<()>>,
}

impl MetricsServer {
    /// Bind to `127.0.0.1:<port>` and start serving in a background thread.
    pub fn start(port: u16) -> Result<Self> {
        let listener = TcpListener::bind(format!("127.0.0.1:{}", port))
            .with_context(|| format!("Failed to bind metrics server on port {}", port))?;
        // Non-blocking accept so the shutdown flag is checked promptly.
        listener
            .set_nonblocking(true)
            .context("Failed to set metrics listener to non-blocking")?;

        let shutdown = Arc::new(AtomicBool::new(false));
        let shutdown_clone = Arc::clone(&shutdown);

        let handle = std::thread::spawn(move || {
            serve_loop(listener, shutdown_clone);
        });

        tracing::info!("Metrics available at http://127.0.0.1:{}/metrics", port);

        Ok(Self {
            shutdown,
            handle: Some(handle),
        })
    }

    /// Signal the background thread to stop and wait for it to exit.
    pub fn stop(mut self) {
        self.shutdown.store(true, Ordering::Relaxed);
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}

fn serve_loop(listener: TcpListener, shutdown: Arc<AtomicBool>) {
    loop {
        if shutdown.load(Ordering::Relaxed) {
            break;
        }
        match listener.accept() {
            Ok((stream, _)) => handle_connection(stream),
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                std::thread::sleep(std::time::Duration::from_millis(50));
            }
            Err(_) => break,
        }
    }
}

fn handle_connection(mut stream: TcpStream) {
    // Read the request line — we don't need to parse it fully.
    let mut buf = [0u8; 512];
    let _ = stream.read(&mut buf);

    let body = mvm_core::observability::metrics::global().prometheus_exposition();
    let response = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: text/plain; version=0.0.4\r\nContent-Length: {}\r\n\r\n{}",
        body.len(),
        body
    );
    let _ = stream.write_all(response.as_bytes());
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_metrics_server_binds() {
        // Pick a port unlikely to be in use; retry once with a different port if needed.
        let server = MetricsServer::start(19091)
            .or_else(|_| MetricsServer::start(19092))
            .expect("metrics server should bind");
        server.stop();
    }

    #[test]
    fn test_metrics_server_responds() {
        use std::io::{BufRead, BufReader, Write};
        use std::net::TcpStream;

        let server = MetricsServer::start(19093)
            .or_else(|_| MetricsServer::start(19094))
            .expect("metrics server should bind");

        // Give the background thread a moment to start.
        std::thread::sleep(std::time::Duration::from_millis(50));

        // Determine which port actually bound by inspecting the local addr.
        // Since we can't easily query the bound port from MetricsServer,
        // try both candidate ports.
        let stream = TcpStream::connect("127.0.0.1:19093")
            .or_else(|_| TcpStream::connect("127.0.0.1:19094"))
            .expect("should connect to metrics server");

        let mut stream_clone = stream.try_clone().unwrap();
        stream_clone
            .write_all(b"GET /metrics HTTP/1.0\r\n\r\n")
            .unwrap();

        let mut reader = BufReader::new(stream);
        let mut response = String::new();
        loop {
            let mut line = String::new();
            if reader.read_line(&mut line).unwrap_or(0) == 0 {
                break;
            }
            response.push_str(&line);
        }

        assert!(
            response.contains("mvm_requests_total"),
            "response should contain prometheus metrics, got: {response}"
        );

        server.stop();
    }
}
