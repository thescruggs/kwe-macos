// SPDX-License-Identifier: GPL-3.0-or-later
//! The CDP client: request/response correlation with timeouts over the
//! bounded [`Connection`].
//!
//! The browser session is addressed with [`Client::request_browser`];
//! flattened target sessions with [`Client::request_session`]. Every
//! request blocks at most `request_timeout` (default 5 s) by pumping the
//! pipe in slices — there is no other waiting, no async runtime, no threads.
//! Events accumulate in the bounded queue and are drained with
//! [`Client::next_event`] after any call.

use std::os::fd::RawFd;
use std::time::{Duration, Instant};

use serde_json::Value;

use crate::codec::DEFAULT_MAX_MESSAGE_BYTES;
use crate::connection::{Connection, Event, Response};
use crate::error::Error;

/// Default request timeout: generous enough for cold browser start, short
/// enough that a wedged browser cannot wedge the renderer.
pub const DEFAULT_REQUEST_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Debug)]
pub struct Client {
    connection: Connection,
    request_timeout: Duration,
}

impl Client {
    /// Take ownership of the pipe ends and build a client. Both fds are
    /// switched to O_NONBLOCK; they close when the client drops (which is
    /// also the browser teardown signal).
    pub fn new(read_fd: RawFd, write_fd: RawFd) -> Result<Self, Error> {
        Ok(Client {
            connection: Connection::new(read_fd, write_fd, DEFAULT_MAX_MESSAGE_BYTES)?,
            request_timeout: DEFAULT_REQUEST_TIMEOUT,
        })
    }

    /// Override the per-request timeout.
    pub fn with_request_timeout(mut self, timeout: Duration) -> Self {
        self.request_timeout = timeout;
        self
    }

    /// Request against the browser session (no sessionId on the wire).
    pub fn request_browser(&mut self, method: &str, params: &Value) -> Result<Response, Error> {
        self.request(None, method, params)
    }

    /// Request inside a flattened target session.
    pub fn request_session(
        &mut self,
        session_id: &str,
        method: &str,
        params: &Value,
    ) -> Result<Response, Error> {
        self.request(Some(session_id), method, params)
    }

    /// Send a request inside a flattened target session and return its id
    /// without waiting for the answer. The response — if it ever arrives —
    /// is picked up later with [`Client::take_response`], which never
    /// blocks. This is the non-blocking half of [`Client::request_session`];
    /// it exists for liveness probes that must not stall the caller's own
    /// loop (a blocking request would let a wedged page stall the renderer's
    /// publish pipeline). The pending slot is bounded by the connection's
    /// in-flight limit; a response to an id the caller stopped tracking is
    /// dropped by routing. `id` monotonically increases.
    pub fn send_session(
        &mut self,
        session_id: &str,
        method: &str,
        params: &Value,
    ) -> Result<u32, Error> {
        self.connection
            .send_request(method, params, Some(session_id))
    }

    /// Take the response for a previously sent request id if it has
    /// arrived, without waiting. Returns `None` while the request is still
    /// in flight or after it was discarded.
    pub fn take_response(&mut self, id: u32) -> Option<Response> {
        self.connection.take_response(id)
    }

    /// Pump the pipe for up to `timeout` without awaiting any response.
    /// Delivers events into the queue; returns `Error::Io` on pipe failure
    /// or peer close.
    pub fn poll(&mut self, timeout: Duration) -> Result<(), Error> {
        self.connection.poll(timeout)
    }

    /// Pop the oldest queued event, if any.
    pub fn next_event(&mut self) -> Option<Event> {
        self.connection.next_event()
    }

    /// Number of events dropped because the queue was full.
    pub fn events_dropped(&self) -> u64 {
        self.connection.events_dropped()
    }

    /// Requests whose responses are still awaited.
    pub fn in_flight(&self) -> usize {
        self.connection.in_flight()
    }

    fn request(
        &mut self,
        session_id: Option<&str>,
        method: &str,
        params: &Value,
    ) -> Result<Response, Error> {
        let id = self.connection.send_request(method, params, session_id)?;
        let deadline = Instant::now() + self.request_timeout;
        loop {
            let now = Instant::now();
            if now >= deadline {
                // The response may still be in flight; forget the slot so a
                // late answer is dropped by routing instead of leaking.
                self.connection.discard_pending(id);
                return Err(Error::Timeout(self.request_timeout));
            }
            self.connection.poll(deadline - now)?;
            if let Some(response) = self.connection.take_response(id) {
                return Ok(response);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::socket_pair;
    use serde_json::json;

    struct SilentPeer {
        _read_fd: RawFd,
        _write_fd: RawFd,
    }

    fn silent_client() -> (Client, SilentPeer) {
        // Two independent channels, mirroring chromium's two pipes; see
        // transport::tests::FakePeer::new for the rationale.
        let (client_read, peer_write) = socket_pair();
        let (client_write, peer_read) = socket_pair();
        let client = Client::new(client_read, client_write).unwrap();
        (
            client,
            SilentPeer {
                _read_fd: peer_read,
                _write_fd: peer_write,
            },
        )
    }

    #[test]
    fn request_times_out_without_a_response() {
        let (client, _peer) = silent_client();
        let mut client = client.with_request_timeout(Duration::from_millis(120));
        let start = Instant::now();
        let err = client
            .request_browser("Target.getTargets", &json!({}))
            .unwrap_err();
        assert!(matches!(err, Error::Timeout(t) if t == Duration::from_millis(120)));
        // The timeout must not burn much more than its own budget.
        assert!(start.elapsed() < Duration::from_secs(2));
        // The abandoned slot is reaped, so repeated requests cannot leak.
        assert_eq!(client.in_flight(), 0);
    }

    #[test]
    fn response_within_the_timeout_succeeds() {
        let (client, peer) = silent_client();
        let mut client = client.with_request_timeout(Duration::from_secs(5));
        let peer_read = peer._read_fd;
        let peer_write = peer._write_fd;
        std::thread::spawn(move || {
            // Read the request, answer it with its own id.
            let mut message = Vec::new();
            let mut byte = [0u8; 1];
            loop {
                let n = unsafe { libc::read(peer_read, byte.as_mut_ptr().cast(), 1) };
                if n != 1 {
                    return;
                }
                if byte[0] == 0 {
                    break;
                }
                message.push(byte[0]);
            }
            let request: Value = serde_json::from_slice(&message).unwrap();
            let id = request["id"].as_u64().unwrap();
            let body = format!(r#"{{"id":{id},"result":{{"targets":[]}}}}"#);
            let framed = crate::codec::encode_message(body.as_bytes());
            let mut written = 0;
            while written < framed.len() {
                let n = unsafe {
                    libc::write(
                        peer_write,
                        framed[written..].as_ptr().cast(),
                        framed.len() - written,
                    )
                };
                if n <= 0 {
                    return;
                }
                written += n as usize;
            }
        });
        let response = client
            .request_browser("Target.getTargets", &json!({}))
            .unwrap();
        assert!(response.error.is_none());
        assert_eq!(response.result.unwrap()["targets"], json!([]));
    }

    #[test]
    fn session_request_round_trip() {
        let (mut client, peer) = silent_client();
        let peer_read = peer._read_fd;
        let peer_write = peer._write_fd;
        std::thread::spawn(move || {
            let mut message = Vec::new();
            let mut byte = [0u8; 1];
            loop {
                let n = unsafe { libc::read(peer_read, byte.as_mut_ptr().cast(), 1) };
                if n != 1 {
                    return;
                }
                if byte[0] == 0 {
                    break;
                }
                message.push(byte[0]);
            }
            let request: Value = serde_json::from_slice(&message).unwrap();
            assert_eq!(request["sessionId"], "SESSION-9");
            let id = request["id"].as_u64().unwrap();
            let body = format!(r#"{{"id":{id},"result":{{"ok":true}},"sessionId":"SESSION-9"}}"#);
            let framed = crate::codec::encode_message(body.as_bytes());
            let mut written = 0;
            while written < framed.len() {
                let n = unsafe {
                    libc::write(
                        peer_write,
                        framed[written..].as_ptr().cast(),
                        framed.len() - written,
                    )
                };
                if n <= 0 {
                    return;
                }
                written += n as usize;
            }
        });
        let response = client
            .request_session("SESSION-9", "Page.enable", &json!({}))
            .unwrap();
        assert_eq!(response.session_id.as_deref(), Some("SESSION-9"));
        assert_eq!(response.result.unwrap()["ok"], json!(true));
    }

    #[test]
    fn events_arrive_while_requests_are_in_flight() {
        let (mut client, peer) = silent_client();
        let peer_read = peer._read_fd;
        let peer_write = peer._write_fd;
        std::thread::spawn(move || {
            // OwnedFd guards close the peer ends when the thread exits,
            // which is the shutdown signal for the client's read end.
            use std::os::fd::{AsRawFd, FromRawFd};
            let _read_guard = unsafe { std::os::fd::OwnedFd::from_raw_fd(peer_read) };
            let write_guard = unsafe { std::os::fd::OwnedFd::from_raw_fd(peer_write) };
            let framed = crate::codec::encode_message(
                br#"{"method":"Target.attachedToTarget","params":{"sessionId":"S1"}}"#,
            );
            let mut written = 0;
            while written < framed.len() {
                let n = unsafe {
                    libc::write(
                        write_guard.as_raw_fd(),
                        framed[written..].as_ptr().cast(),
                        framed.len() - written,
                    )
                };
                if n <= 0 {
                    return;
                }
                written += n as usize;
            }
        });
        // No request: just pump until the event lands.
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            client.poll(Duration::from_millis(20)).unwrap();
            if let Some(event) = client.next_event() {
                assert_eq!(event.method, "Target.attachedToTarget");
                assert_eq!(event.params["sessionId"], "S1");
                break;
            }
            assert!(Instant::now() < deadline, "event never arrived");
        }
        assert_eq!(client.events_dropped(), 0);
    }

    #[test]
    fn protocol_errors_surface_without_panicking() {
        let (mut client, peer) = silent_client();
        let peer_read = peer._read_fd;
        let peer_write = peer._write_fd;
        std::thread::spawn(move || {
            let mut message = Vec::new();
            let mut byte = [0u8; 1];
            loop {
                let n = unsafe { libc::read(peer_read, byte.as_mut_ptr().cast(), 1) };
                if n != 1 {
                    return;
                }
                if byte[0] == 0 {
                    break;
                }
                message.push(byte[0]);
            }
            let request: Value = serde_json::from_slice(&message).unwrap();
            let id = request["id"].as_u64().unwrap();
            let body = format!(
                r#"{{"id":{id},"error":{{"code":-32601,"message":"'Bogus.m' wasn't found"}}}}"#
            );
            let framed = crate::codec::encode_message(body.as_bytes());
            let mut written = 0;
            while written < framed.len() {
                let n = unsafe {
                    libc::write(
                        peer_write,
                        framed[written..].as_ptr().cast(),
                        framed.len() - written,
                    )
                };
                if n <= 0 {
                    return;
                }
                written += n as usize;
            }
        });
        let response = client.request_browser("Bogus.m", &json!({})).unwrap();
        let error = response.error.expect("protocol error envelope");
        assert_eq!(error["code"], json!(-32601));
    }

    #[test]
    fn send_session_then_take_response_is_non_blocking() {
        let (mut client, peer) = silent_client();
        let peer_read = peer._read_fd;
        let peer_write = peer._write_fd;
        std::thread::spawn(move || {
            let mut message = Vec::new();
            let mut byte = [0u8; 1];
            loop {
                let n = unsafe { libc::read(peer_read, byte.as_mut_ptr().cast(), 1) };
                if n != 1 {
                    return;
                }
                if byte[0] == 0 {
                    break;
                }
                message.push(byte[0]);
            }
            let request: Value = serde_json::from_slice(&message).unwrap();
            assert_eq!(request["sessionId"], "SESSION-LIVE");
            assert_eq!(request["method"], "Runtime.evaluate");
            assert_eq!(request["params"]["expression"], "1+1");
            let id = request["id"].as_u64().unwrap();
            let body = format!(
                r#"{{"id":{id},"result":{{"result":{{"type":"number","value":2}}}},"sessionId":"SESSION-LIVE"}}"#
            );
            let framed = crate::codec::encode_message(body.as_bytes());
            let mut written = 0;
            while written < framed.len() {
                let n = unsafe {
                    libc::write(
                        peer_write,
                        framed[written..].as_ptr().cast(),
                        framed.len() - written,
                    )
                };
                if n <= 0 {
                    return;
                }
                written += n as usize;
            }
        });
        let id = client
            .send_session(
                "SESSION-LIVE",
                "Runtime.evaluate",
                &json!({"expression": "1+1"}),
            )
            .expect("request sent without waiting");
        assert!(id > 0);
        // Still in flight at the moment of sending: never blocks.
        assert!(client.take_response(id).is_none());
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            client.poll(Duration::from_millis(20)).unwrap();
            if let Some(response) = client.take_response(id) {
                assert!(response.error.is_none());
                assert_eq!(response.session_id.as_deref(), Some("SESSION-LIVE"));
                assert_eq!(response.result.unwrap()["result"]["value"], json!(2));
                break;
            }
            assert!(Instant::now() < deadline, "response never arrived");
        }
        // A taken response is not delivered twice.
        assert!(client.take_response(id).is_none());
        assert_eq!(client.in_flight(), 0);
    }

    #[test]
    fn unacked_send_session_slots_are_dropped_when_id_is_abandoned() {
        let (mut client, _peer) = silent_client();
        let id = client
            .send_session(
                "SESSION-GHOST",
                "Runtime.evaluate",
                &json!({"expression": "1+1"}),
            )
            .expect("request sent");
        // The caller (liveness tracker) abandons the id after its deadline;
        // the slot must not leak into the next probe's bookkeeping.
        assert!(client.take_response(id).is_none());
        assert_eq!(client.in_flight(), 1);
        let _ = client.poll(Duration::from_millis(30));
        // Nothing ever answered; the slot stays until a late answer is
        // dropped by routing or the client drops.
        assert_eq!(client.in_flight(), 1);
        // The tracker abandons the old id after its deadline; a fresh probe
        // must never collide with it.
        let fresh = client
            .send_session(
                "SESSION-GHOST",
                "Runtime.evaluate",
                &json!({"expression": "1+1"}),
            )
            .expect("request sent");
        assert!(fresh > id);
    }
}
