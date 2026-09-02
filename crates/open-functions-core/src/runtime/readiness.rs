//! TCP-connect readiness polling.
//!
//! The Functions Framework Contract does not define an HTTP readiness
//! endpoint: a function instance is considered ready once it is listening on
//! `PORT` at all. A raw TCP connect is therefore the readiness signal.

use std::net::SocketAddr;
use std::time::Duration;

use tokio::net::TcpStream;
use tokio::time::Instant;

/// The interval between readiness poll attempts.
const POLL_INTERVAL: Duration = Duration::from_millis(500);

/// Polls `addr` with a plain TCP connect every 500ms until one succeeds or
/// `timeout` elapses. A successful TCP connect (regardless of what, if
/// anything, is sent/received) is the readiness signal.
///
/// Returns `Err(timeout)` if the instance never became ready within `timeout`.
pub async fn wait_ready(addr: SocketAddr, timeout: Duration) -> Result<(), Duration> {
    let deadline = Instant::now() + timeout;

    loop {
        if TcpStream::connect(addr).await.is_ok() {
            return Ok(());
        }

        let now = Instant::now();
        if now >= deadline {
            return Err(timeout);
        }

        let remaining = deadline.saturating_duration_since(now);
        tokio::time::sleep(remaining.min(POLL_INTERVAL)).await;
    }
}
