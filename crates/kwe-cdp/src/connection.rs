// SPDX-License-Identifier: Apache-2.0
//! CDP message correlation: requests carry monotonic u32 ids; responses are
//! matched back to their pending slot, events land in a bounded queue.
//!
//! The wire envelope (pinned empirically in `docs/BETA_M2.md`):
//!
//! ```text
//! request : {"id":<u32>,"method":"...","params":{...}[,"sessionId":"..."]}
//! response: {"id":<u32>[,"result":{...}|"error":{code,message}][,"sessionId":"..."]}
//! event   : {"method":"...","params":{...}[,"sessionId":"..."]}
//! ```
//!
//! The browser session carries no sessionId; flattened target sessions
//! (`Target.attachToTarget` with `flatten: true`) stamp every response and
//! event with their sessionId. Messages that cannot be matched — responses
//! for ids we never sent, events past the queue bound — are dropped, not
//! buffered without limit.

use std::collections::VecDeque;
use std::io;
use std::os::fd::RawFd;
use std::time::{Duration, Instant};

use serde_json::{Value, json};

use crate::error::Error;
use crate::transport::PipeTransport;

/// Maximum events buffered before the oldest are dropped (drop-oldest),
/// counted in `events_dropped`.
pub const DEFAULT_EVENT_QUEUE_LIMIT: usize = 64;

/// Maximum in-flight requests whose responses we still await. The client
/// removes slots on response or timeout, so this only trips when a caller
/// forgets to reap.
pub const MAX_IN_FLIGHT_REQUESTS: usize = 64;

/// A CDP response envelope, matched to the request that produced it.
#[derive(Debug, Clone, PartialEq)]
pub struct Response {
    /// Id of the request this answers.
    pub id: u32,
    /// Session the response belongs to (`None` for the browser session).
    pub session_id: Option<String>,
    /// `result` payload, absent when the method failed.
    pub result: Option<Value>,
    /// `{code, message}` protocol error, present when the method failed.
    pub error: Option<Value>,
}

/// A CDP event, delivered to the bounded queue.
#[derive(Debug, Clone, PartialEq)]
pub struct Event {
    pub method: String,
    pub params: Value,
    /// Session this event belongs to (`None` for the browser session).
    pub session_id: Option<String>,
}

/// Correlates requests and responses over a [`PipeTransport`] and routes
/// events into a bounded queue.
#[derive(Debug)]
pub struct Connection {
    transport: PipeTransport,
    next_id: u32,
    pending: Vec<(u32, Instant, Option<Response>)>,
    events: VecDeque<Event>,
    events_dropped: u64,
}

impl Connection {
    /// Take ownership of `read_fd`/`write_fd` (nonblocking) and build the
    /// transport with a per-message bound of `max_message_bytes`.
    pub fn new(read_fd: RawFd, write_fd: RawFd, max_message_bytes: usize) -> Result<Self, Error> {
        Ok(Connection {
            transport: PipeTransport::new(read_fd, write_fd, max_message_bytes)?,
            next_id: 0,
            pending: Vec::new(),
            events: VecDeque::new(),
            events_dropped: 0,
        })
    }

    /// Send a request and return its id. The response arrives later via
    /// [`Connection::poll`] + [`Connection::take_response`].
    pub fn send_request(
        &mut self,
        method: &str,
        params: &Value,
        session_id: Option<&str>,
    ) -> Result<u32, Error> {
        if self.pending.len() >= MAX_IN_FLIGHT_REQUESTS {
            return Err(Error::Io(io::Error::new(
                io::ErrorKind::WouldBlock,
                "too many in-flight CDP requests",
            )));
        }
        let id = self.alloc_id();
        let mut request = json!({ "id": id, "method": method, "params": params });
        if let Some(session_id) = session_id {
            request["sessionId"] = Value::String(session_id.to_owned());
        }
        let body = serde_json::to_vec(&request).map_err(|e| Error::ParseError(e.to_string()))?;
        self.transport.send(&body)?;
        self.pending.push((id, Instant::now(), None));
        Ok(id)
    }

    /// Pump the pipe once within `timeout`; routes responses to pending
    /// slots and events to the queue. A zero timeout never blocks.
    pub fn poll(&mut self, timeout: Duration) -> Result<(), Error> {
        for body in self.transport.poll(timeout)? {
            self.route_message(&body)?;
        }
        Ok(())
    }

    /// Take the response for `id`, removing its pending slot only when the
    /// response has actually arrived. Returns `None` while the response is
    /// still in flight, leaving the slot in place.
    pub fn take_response(&mut self, id: u32) -> Option<Response> {
        let index = self
            .pending
            .iter()
            .position(|(pending_id, _, _)| *pending_id == id)?;
        self.pending[index].2.as_ref()?;
        let (_, _, response) = self.pending.swap_remove(index);
        response
    }

    /// Forget a pending request without awaiting its response (timeout
    /// path). The slot's eventual response, if any, is dropped by routing.
    pub fn discard_pending(&mut self, id: u32) {
        if let Some(index) = self
            .pending
            .iter()
            .position(|(pending_id, _, _)| *pending_id == id)
        {
            self.pending.swap_remove(index);
        }
    }

    /// Pop the oldest queued event, if any.
    pub fn next_event(&mut self) -> Option<Event> {
        self.events.pop_front()
    }

    /// Number of events dropped because the queue was full.
    pub fn events_dropped(&self) -> u64 {
        self.events_dropped
    }

    /// Requests whose responses are still awaited.
    pub fn in_flight(&self) -> usize {
        self.pending.len()
    }

    /// Events currently buffered in the queue.
    pub fn queued_events(&self) -> usize {
        self.events.len()
    }

    fn alloc_id(&mut self) -> u32 {
        // Monotonic u32; wraps past u32::MAX but never reuses 0.
        self.next_id = self.next_id.wrapping_add(1);
        if self.next_id == 0 {
            self.next_id = 1;
        }
        self.next_id
    }

    fn route_message(&mut self, body: &[u8]) -> Result<(), Error> {
        let value: Value =
            serde_json::from_slice(body).map_err(|e| Error::ParseError(e.to_string()))?;
        let session_id = value
            .get("sessionId")
            .and_then(Value::as_str)
            .map(str::to_owned);
        if value.get("id").is_some() {
            // Response or error envelope: both carry the request id.
            let id = u32::try_from(
                value
                    .get("id")
                    .and_then(Value::as_u64)
                    .ok_or_else(|| Error::ParseError("response id is not an integer".into()))?,
            )
            .map_err(|_| Error::ParseError("response id out of range".into()))?;
            if let Some(slot) = self
                .pending
                .iter_mut()
                .find(|(pending_id, _, _)| *pending_id == id)
            {
                slot.2 = Some(Response {
                    id,
                    session_id,
                    result: value.get("result").cloned(),
                    error: value.get("error").cloned(),
                });
            }
            // Unknown id: unsolicited response (peer chatter after our
            // timeout); dropped, bounded by construction.
        } else {
            let method = value
                .get("method")
                .and_then(Value::as_str)
                .ok_or_else(|| Error::ParseError("message has neither id nor method".into()))?
                .to_owned();
            let params = value.get("params").cloned().unwrap_or(Value::Null);
            self.enqueue_event(Event {
                method,
                params,
                session_id,
            });
        }
        Ok(())
    }

    fn enqueue_event(&mut self, event: Event) {
        if self.events.len() >= DEFAULT_EVENT_QUEUE_LIMIT {
            self.events.pop_front();
            self.events_dropped += 1;
        }
        self.events.push_back(event);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::socket_pair;

    struct FakePeer {
        read_fd: RawFd,
        write_fd: RawFd,
    }

    impl Drop for FakePeer {
        fn drop(&mut self) {
            // RawFd is a plain integer: closing the peer's ends on drop is
            // what makes `drop(peer)` behave like a peer that went away.
            unsafe {
                libc::close(self.read_fd);
                libc::close(self.write_fd);
            }
        }
    }

    impl FakePeer {
        fn new() -> (Connection, FakePeer) {
            // Two independent channels, mirroring chromium's two pipes;
            // see transport::tests::FakePeer::new for the rationale.
            let (client_read, peer_write) = socket_pair();
            let (client_write, peer_read) = socket_pair();
            let connection = Connection::new(
                client_read,
                client_write,
                crate::codec::DEFAULT_MAX_MESSAGE_BYTES,
            )
            .unwrap();
            (
                connection,
                FakePeer {
                    read_fd: peer_read,
                    write_fd: peer_write,
                },
            )
        }

        fn read_message(&self) -> Value {
            let mut message = Vec::new();
            let mut byte = [0u8; 1];
            loop {
                let n = unsafe { libc::read(self.read_fd, byte.as_mut_ptr().cast(), 1) };
                assert_eq!(n, 1, "peer read failed");
                if byte[0] == 0 {
                    break;
                }
                message.push(byte[0]);
            }
            serde_json::from_slice(&message).expect("peer saw invalid JSON")
        }

        fn write_body(&self, body: &str) {
            let framed = encode_body(body);
            let mut written = 0;
            while written < framed.len() {
                let n = unsafe {
                    libc::write(
                        self.write_fd,
                        framed[written..].as_ptr().cast(),
                        framed.len() - written,
                    )
                };
                assert!(n > 0, "peer write failed");
                written += n as usize;
            }
        }

        fn respond(&self, id: u32, session_id: Option<&str>, error: bool) {
            let body = match (session_id, error) {
                (Some(sid), false) => {
                    format!(r#"{{"id":{id},"result":{{"ok":true,"v":{id}}},"sessionId":"{sid}"}}"#)
                }
                (None, false) => format!(r#"{{"id":{id},"result":{{"ok":true,"v":{id}}}}}"#),
                (Some(sid), true) => format!(
                    r#"{{"id":{id},"error":{{"code":-32601,"message":"x"}},"sessionId":"{sid}"}}"#
                ),
                (None, true) => {
                    format!(r#"{{"id":{id},"error":{{"code":-32601,"message":"x"}}}}"#)
                }
            };
            self.write_body(&body);
        }

        fn send_event(&self, method: &str, session_id: Option<&str>) {
            let body = match session_id {
                Some(sid) => {
                    format!(r#"{{"method":"{method}","params":{{"n":1}},"sessionId":"{sid}"}}"#)
                }
                None => format!(r#"{{"method":"{method}","params":{{"n":1}}}}"#),
            };
            self.write_body(&body);
        }
    }

    fn encode_body(body: &str) -> Vec<u8> {
        crate::codec::encode_message(body.as_bytes())
    }

    fn pump_until(
        connection: &mut Connection,
        deadline: Duration,
        mut done: impl FnMut(&mut Connection) -> bool,
    ) {
        let start = Instant::now();
        loop {
            if done(connection) {
                return;
            }
            if start.elapsed() >= deadline {
                panic!("timed out waiting for the fake peer");
            }
            connection.poll(Duration::from_millis(10)).unwrap();
        }
    }

    #[test]
    fn request_ids_are_monotonic_and_never_zero() {
        let (mut connection, peer) = FakePeer::new();
        let id1 = connection.send_request("A.m", &json!({}), None).unwrap();
        assert!(id1 != 0);
        let id2 = connection.send_request("A.m", &json!({}), None).unwrap();
        assert!(id2 > id1);
        drop(peer);
    }

    #[test]
    fn responses_correlate_by_id_in_any_order() {
        let (mut connection, peer) = FakePeer::new();
        let id1 = connection.send_request("A.m", &json!({}), None).unwrap();
        let id2 = connection.send_request("B.m", &json!({}), None).unwrap();
        // Answer the second request first.
        peer.respond(id2, None, false);
        peer.respond(id1, None, false);
        let mut response1 = None;
        let mut response2 = None;
        pump_until(&mut connection, Duration::from_secs(5), |connection| {
            if response1.is_none() {
                response1 = connection.take_response(id1);
            }
            if response2.is_none() {
                response2 = connection.take_response(id2);
            }
            response1.is_some() && response2.is_some()
        });
        let response1 = response1.expect("response 1");
        let response2 = response2.expect("response 2");
        assert_eq!(response1.id, id1);
        assert_eq!(response1.result.as_ref().unwrap()["v"], json!(id1));
        assert_eq!(response2.result.as_ref().unwrap()["v"], json!(id2));
        assert_eq!(connection.in_flight(), 0);
    }

    #[test]
    fn error_envelopes_arrive_as_responses() {
        let (mut connection, peer) = FakePeer::new();
        let id = connection
            .send_request("Bogus.m", &json!({}), None)
            .unwrap();
        peer.respond(id, None, true);
        let mut response = None;
        pump_until(&mut connection, Duration::from_secs(5), |connection| {
            if response.is_none() {
                response = connection.take_response(id);
            }
            response.is_some()
        });
        let response = response.expect("error response");
        assert!(response.result.is_none());
        let error = response.error.expect("error envelope");
        assert_eq!(error["code"], json!(-32601));
        assert_eq!(error["message"], "x");
    }

    #[test]
    fn session_scoped_requests_carry_session_id() {
        let (mut connection, peer) = FakePeer::new();
        let id = connection
            .send_request("Page.enable", &json!({}), Some("SESSION-1"))
            .unwrap();
        let request = peer.read_message();
        assert_eq!(request["id"], json!(id));
        assert_eq!(request["sessionId"], "SESSION-1");
        assert_eq!(request["method"], "Page.enable");
        peer.respond(id, Some("SESSION-1"), false);
        let mut response = None;
        pump_until(&mut connection, Duration::from_secs(5), |connection| {
            if response.is_none() {
                response = connection.take_response(id);
            }
            response.is_some()
        });
        let response = response.expect("session response");
        assert_eq!(response.session_id.as_deref(), Some("SESSION-1"));
    }

    #[test]
    fn unsolicited_responses_are_dropped() {
        let (mut connection, peer) = FakePeer::new();
        // A response for an id we never sent must not crash or enqueue.
        peer.respond(999, None, false);
        connection.poll(Duration::from_secs(5)).unwrap();
        assert_eq!(connection.queued_events(), 0);
        assert_eq!(connection.in_flight(), 0);
    }

    #[test]
    fn events_route_with_session_id() {
        let (mut connection, peer) = FakePeer::new();
        peer.send_event("Target.attachedToTarget", None);
        peer.send_event("Page.screencastFrame", Some("SESSION-1"));
        pump_until(&mut connection, Duration::from_secs(5), |connection| {
            connection.queued_events() == 2
        });
        let browser_event = connection.next_event().unwrap();
        assert_eq!(browser_event.method, "Target.attachedToTarget");
        assert_eq!(browser_event.session_id, None);
        let session_event = connection.next_event().unwrap();
        assert_eq!(session_event.method, "Page.screencastFrame");
        assert_eq!(session_event.session_id.as_deref(), Some("SESSION-1"));
    }

    #[test]
    fn event_queue_is_bounded_and_counts_drops() {
        let (mut connection, peer) = FakePeer::new();
        for i in 0..(DEFAULT_EVENT_QUEUE_LIMIT + 36) {
            peer.send_event(&format!("M.e{i}"), None);
        }
        pump_until(&mut connection, Duration::from_secs(5), |connection| {
            connection.queued_events() == DEFAULT_EVENT_QUEUE_LIMIT
        });
        assert_eq!(connection.events_dropped(), 36);
        // Drop-oldest: the first event kept must be #36 (36 dropped).
        let first_kept = connection.next_event().unwrap();
        assert_eq!(first_kept.method, "M.e36");
        let mut seen = 1;
        while connection.next_event().is_some() {
            seen += 1;
        }
        assert_eq!(seen, DEFAULT_EVENT_QUEUE_LIMIT);
    }

    #[test]
    fn discard_pending_frees_the_slot() {
        let (mut connection, peer) = FakePeer::new();
        let id = connection.send_request("A.m", &json!({}), None).unwrap();
        assert_eq!(connection.in_flight(), 1);
        connection.discard_pending(id);
        assert_eq!(connection.in_flight(), 0);
        // A late response for the discarded id is dropped, not re-queued.
        peer.respond(id, None, false);
        pump_until(&mut connection, Duration::from_secs(5), |connection| {
            connection.queued_events() == 0 && connection.in_flight() == 0
        });
        drop(peer);
    }

    #[test]
    fn parse_errors_surface_on_garbage() {
        let (mut connection, peer) = FakePeer::new();
        peer.write_body("{not json");
        let err = connection.poll(Duration::from_secs(5)).unwrap_err();
        assert!(matches!(err, Error::ParseError(_)));
    }

    #[test]
    fn message_with_neither_id_nor_method_is_a_parse_error() {
        let (mut connection, peer) = FakePeer::new();
        peer.write_body(r#"{"params":{}}"#);
        let err = connection.poll(Duration::from_secs(5)).unwrap_err();
        assert!(matches!(err, Error::ParseError(_)));
    }

    #[test]
    fn pending_survives_an_early_take_response() {
        // Regression: take_response must not remove the slot while the
        // response is still in flight, or a caller that probes early (then
        // pumps) would lose the slot and the late response would be dropped
        // as unsolicited.
        let (mut connection, peer) = FakePeer::new();
        let id = connection.send_request("A.m", &json!({}), None).unwrap();
        assert_eq!(connection.in_flight(), 1);
        assert!(connection.take_response(id).is_none());
        assert_eq!(
            connection.in_flight(),
            1,
            "slot must survive the early take"
        );
        peer.respond(id, None, false);
        pump_until(&mut connection, Duration::from_secs(5), |connection| {
            connection.take_response(id).is_some()
        });
        assert_eq!(connection.in_flight(), 0);
    }
}
