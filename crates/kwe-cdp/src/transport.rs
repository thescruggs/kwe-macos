// SPDX-License-Identifier: GPL-3.0-or-later
//! Nonblocking pipe transport over the fds the renderer hands the browser.
//!
//! Chromium reads CDP messages from fd 3 and writes them to fd 4
//! (`--remote-debugging-pipe`); the client owns the opposite ends of those
//! pipes. All I/O here is nonblocking with `poll(2)` deadlines, so a wedged
//! peer can never block the renderer loop. Writes are buffered in a bounded
//! backlog: if the peer stops draining, `poll` surfaces an I/O error instead
//! of growing memory. Reads are capped per call (64 KiB budget) and decoded
//! by the shared ASCIIZ [`crate::codec::Decoder`] with its 4 MiB per-message
//! bound.

use std::io;
use std::os::fd::RawFd;
use std::time::{Duration, Instant};

use crate::codec::{Decoder, encode_message};
use crate::error::Error;

/// Bytes read from the pipe per `poll` call, at most. Bounds per-call work
/// so a fast peer cannot starve the rest of the renderer loop.
pub const DEFAULT_READ_BUDGET_BYTES: usize = 64 * 1024;

/// Upper bound on buffered unwritten request bytes. Exceeding it means the
/// peer is not draining its read end at all.
pub const MAX_WRITE_BACKLOG_BYTES: usize = 4 * 1024 * 1024;

/// Chunk size for each `read(2)`.
const READ_CHUNK_BYTES: usize = 4096;

/// A nonblocking CDP pipe transport, owning its two file descriptors.
///
/// Closes both fds on drop, which is also the browser teardown signal:
/// chromium exits within ~50 ms once the client closes the pipe ends
/// (pinned in `docs/BETA_M2.md`).
#[derive(Debug)]
pub struct PipeTransport {
    read_fd: RawFd,
    write_fd: RawFd,
    max_message_bytes: usize,
    decoder: Decoder,
    write_backlog: Vec<u8>,
    write_offset: usize,
    read_bytes: u64,
    written_bytes: u64,
    /// Set when the read side hit EOF. A poll that decoded messages before
    /// the EOF delivers them first; the next poll surfaces the error.
    eof_seen: bool,
}

impl PipeTransport {
    /// Take ownership of `read_fd`/`write_fd` and switch them to O_NONBLOCK.
    pub fn new(read_fd: RawFd, write_fd: RawFd, max_message_bytes: usize) -> Result<Self, Error> {
        set_nonblocking(read_fd)?;
        set_nonblocking(write_fd)?;
        Ok(PipeTransport {
            read_fd,
            write_fd,
            max_message_bytes,
            decoder: Decoder::new(max_message_bytes),
            write_backlog: Vec::new(),
            write_offset: 0,
            read_bytes: 0,
            written_bytes: 0,
            eof_seen: false,
        })
    }

    /// Flush pending writes, then read whatever arrived within `timeout`,
    /// and return the decoded messages.
    ///
    /// A zero timeout never blocks: one nonblocking attempt in each
    /// direction. A positive timeout that expires while the write backlog is
    /// still not drained returns `Error::Io(WouldBlock)` — the peer is
    /// wedged.
    pub fn poll(&mut self, timeout: Duration) -> Result<Vec<Vec<u8>>, Error> {
        if self.eof_seen {
            // A previous poll delivered the last messages and hit EOF; the
            // error is now due (chromium exits right after closing the pipe).
            return Err(self.eof_error());
        }
        let blocking = !timeout.is_zero();
        let deadline = Instant::now() + timeout;
        self.flush_writes(deadline, blocking)?;
        let remaining = deadline.saturating_duration_since(Instant::now());
        self.read_available(remaining)?;
        Ok(self.decoder.take_messages())
    }

    /// Send one message body (framed with its NUL terminator). Bounded by
    /// the per-message bound (body + NUL must fit, mirroring the decoder's
    /// bound) and the write-backlog bound; never blocks — unwritten bytes
    /// wait in the backlog until the next `poll`.
    pub fn send(&mut self, body: &[u8]) -> Result<(), Error> {
        if body.len() + 1 > self.max_message_bytes {
            return Err(Error::OversizedMessage(self.max_message_bytes));
        }
        let pending = self.write_backlog.len() - self.write_offset;
        if pending + body.len() + 1 > MAX_WRITE_BACKLOG_BYTES {
            return Err(Error::Io(io::Error::new(
                io::ErrorKind::WouldBlock,
                "CDP write backlog full: peer is not draining the pipe",
            )));
        }
        self.write_backlog.extend_from_slice(&encode_message(body));
        // Best-effort immediate flush; `poll` finishes the rest.
        let _ = self.flush_writes(Instant::now(), false);
        Ok(())
    }

    /// Bytes of unwritten requests still buffered.
    pub fn pending_write_bytes(&self) -> usize {
        self.write_backlog.len() - self.write_offset
    }

    /// Total bytes read from the pipe since construction.
    pub fn read_bytes(&self) -> u64 {
        self.read_bytes
    }

    /// Total bytes written to the pipe since construction.
    pub fn written_bytes(&self) -> u64 {
        self.written_bytes
    }

    /// Per-message bound of this transport.
    pub fn max_message_bytes(&self) -> usize {
        self.max_message_bytes
    }

    fn flush_writes(&mut self, deadline: Instant, blocking: bool) -> Result<(), Error> {
        while self.write_offset < self.write_backlog.len() {
            let written = unsafe {
                libc::write(
                    self.write_fd,
                    self.write_backlog[self.write_offset..].as_ptr().cast(),
                    self.write_backlog.len() - self.write_offset,
                )
            };
            if written > 0 {
                let written = written as usize;
                self.write_offset += written;
                self.written_bytes += written as u64;
                continue;
            }
            if written == 0 {
                return Err(Error::Io(io::Error::new(
                    io::ErrorKind::WriteZero,
                    "CDP pipe accepted a zero-byte write",
                )));
            }
            let err = io::Error::last_os_error();
            match err.kind() {
                io::ErrorKind::Interrupted => continue,
                io::ErrorKind::WouldBlock => {
                    if !blocking {
                        // Deliberate zero timeout: defer the backlog to the
                        // next poll instead of blocking.
                        return Ok(());
                    }
                    if Instant::now() >= deadline {
                        // The peer never drained within the window: wedged.
                        return Err(Error::Io(io::Error::new(
                            io::ErrorKind::WouldBlock,
                            "CDP peer is not draining the pipe within the deadline",
                        )));
                    }
                    self.poll_fd(self.write_fd, libc::POLLOUT, deadline)?;
                }
                _ => return Err(Error::Io(err)),
            }
        }
        self.write_backlog.clear();
        self.write_offset = 0;
        Ok(())
    }

    fn read_available(&mut self, timeout: Duration) -> Result<(), Error> {
        if !timeout.is_zero() {
            let ready = self.poll_fd(self.read_fd, libc::POLLIN, Instant::now() + timeout)?;
            if !ready {
                // Nothing arrived within the window: that is the answer.
                return Ok(());
            }
        }
        let mut chunk = [0u8; READ_CHUNK_BYTES];
        let mut budget = DEFAULT_READ_BUDGET_BYTES;
        while budget > 0 {
            let n = unsafe { libc::read(self.read_fd, chunk.as_mut_ptr().cast(), chunk.len()) };
            if n > 0 {
                let n = n as usize;
                self.read_bytes += n as u64;
                self.decoder.push(&chunk[..n])?;
                budget -= n;
                continue;
            }
            if n == 0 {
                // EOF: deliver whatever was decoded in this call first; the
                // next poll() surfaces the closed pipe.
                self.eof_seen = true;
                break;
            }
            let err = io::Error::last_os_error();
            match err.kind() {
                io::ErrorKind::Interrupted => continue,
                io::ErrorKind::WouldBlock => break,
                _ => return Err(Error::Io(err)),
            }
        }
        Ok(())
    }

    fn eof_error(&self) -> Error {
        Error::Io(io::Error::new(
            io::ErrorKind::UnexpectedEof,
            "CDP peer closed the pipe",
        ))
    }

    /// `poll(2)` on one fd until `deadline`; returns true when the requested
    /// event fired, false on a clean timeout. EINTR retries.
    fn poll_fd(
        &mut self,
        fd: RawFd,
        events: libc::c_short,
        deadline: Instant,
    ) -> Result<bool, Error> {
        let mut pollfd = libc::pollfd {
            fd,
            events,
            revents: 0,
        };
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            let timeout_ms = std::cmp::min(remaining.as_millis(), i32::MAX as u128) as i32;
            let rc = unsafe { libc::poll(&mut pollfd, 1, timeout_ms) };
            if rc < 0 {
                let err = io::Error::last_os_error();
                if err.kind() == io::ErrorKind::Interrupted {
                    continue;
                }
                return Err(Error::Io(err));
            }
            return Ok(rc > 0);
        }
    }
}

impl Drop for PipeTransport {
    fn drop(&mut self) {
        // Closing both pipe ends is the teardown signal: chromium exits
        // rc=0 within ~50 ms (pinned in docs/BETA_M2.md).
        unsafe {
            libc::close(self.read_fd);
            libc::close(self.write_fd);
        }
    }
}

fn set_nonblocking(fd: RawFd) -> Result<(), Error> {
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
    if flags < 0 {
        return Err(Error::Io(io::Error::last_os_error()));
    }
    if unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) } < 0 {
        return Err(Error::Io(io::Error::last_os_error()));
    }
    Ok(())
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
        fn new() -> (PipeTransport, FakePeer) {
            // Two independent channels, mirroring chromium's two pipes:
            // client-write -> peer-read and peer-write -> client-read. A
            // single socketpair would loop the client's own writes back to
            // its read end, which real pipes never do.
            let (client_read, peer_write) = socket_pair();
            let (client_write, peer_read) = socket_pair();
            let transport = PipeTransport::new(
                client_read,
                client_write,
                crate::codec::DEFAULT_MAX_MESSAGE_BYTES,
            )
            .unwrap();
            (
                transport,
                FakePeer {
                    read_fd: peer_read,
                    write_fd: peer_write,
                },
            )
        }

        fn read_message(&self) -> Vec<u8> {
            let mut message = Vec::new();
            let mut byte = [0u8; 1];
            loop {
                let n = unsafe { libc::read(self.read_fd, byte.as_mut_ptr().cast(), 1) };
                assert_eq!(n, 1, "peer read failed");
                if byte[0] == 0 {
                    return message;
                }
                message.push(byte[0]);
            }
        }

        fn write_body(&self, body: &[u8]) {
            let framed = encode_message(body);
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

        fn echo_response(&self, id: u32, session_id: Option<&str>) {
            let body = match session_id {
                Some(sid) => format!(r#"{{"id":{id},"result":{{"ok":true}},"sessionId":"{sid}"}}"#),
                None => format!(r#"{{"id":{id},"result":{{"ok":true}}}}"#),
            };
            self.write_body(body.as_bytes());
        }
    }

    #[test]
    fn send_and_poll_round_trip() {
        let (mut transport, peer) = FakePeer::new();
        transport.send(b"{\"id\":1}").unwrap();
        peer.echo_response(1, None);
        let messages = transport.poll(Duration::from_millis(2000)).unwrap();
        assert_eq!(messages, vec![br#"{"id":1,"result":{"ok":true}}"#.to_vec()]);
        assert!(transport.pending_write_bytes() == 0);
    }

    #[test]
    fn send_flushes_without_blocking() {
        let (mut transport, peer) = FakePeer::new();
        transport.send(b"{\"id\":1}").unwrap();
        // The immediate best-effort flush in send() should have delivered
        // the request; the peer sees it without any poll().
        assert_eq!(peer.read_message(), b"{\"id\":1}");
        peer.echo_response(1, None);
        let messages = transport.poll(Duration::ZERO).unwrap();
        assert_eq!(messages.len(), 1);
    }

    #[test]
    fn zero_timeout_poll_never_blocks() {
        let (mut transport, _peer) = FakePeer::new();
        let before = Instant::now();
        let messages = transport.poll(Duration::ZERO).unwrap();
        assert!(messages.is_empty());
        assert!(before.elapsed() < Duration::from_millis(100));
    }

    #[test]
    fn multiple_messages_per_poll() {
        let (mut transport, peer) = FakePeer::new();
        for id in 1..=5 {
            transport
                .send(format!(r#"{{"id":{id}}}"#).as_bytes())
                .unwrap();
            peer.echo_response(id, None);
        }
        let messages = transport.poll(Duration::from_millis(2000)).unwrap();
        assert_eq!(messages.len(), 5);
    }

    #[test]
    fn oversized_send_is_rejected() {
        let (mut transport, _peer) = FakePeer::new();
        let err = transport
            .send(&vec![b'x'; transport.max_message_bytes() + 1])
            .unwrap_err();
        assert!(matches!(err, Error::OversizedMessage(_)));
    }

    #[test]
    fn wedged_peer_backlog_is_bounded() {
        // The peer never reads: writes accumulate in the backlog until the
        // bound trips, without ever blocking.
        let (mut transport, _peer) = FakePeer::new();
        let body = vec![b'x'; 64 * 1024];
        let mut hit_bound = false;
        while !hit_bound {
            match transport.send(&body) {
                Ok(()) => {}
                Err(Error::Io(ref e)) if e.kind() == io::ErrorKind::WouldBlock => hit_bound = true,
                Err(e) => panic!("unexpected error: {e}"),
            }
        }
        // The bound trip must never have let the backlog exceed 4 MiB, and
        // some bytes must actually have accumulated.
        assert!(transport.pending_write_bytes() <= MAX_WRITE_BACKLOG_BYTES);
        assert!(transport.pending_write_bytes() > 0);
    }

    #[test]
    fn wedged_peer_blocking_poll_surfaces_wouldblock() {
        // Regression: a positive-timeout poll whose writes cannot drain must
        // report the wedged peer (docs promise Error::Io(WouldBlock)), not
        // return clean forever.
        let (mut transport, _peer) = FakePeer::new();
        let body = vec![b'x'; 64 * 1024];
        while transport.send(&body).is_ok() {}
        assert!(transport.pending_write_bytes() > 0);
        let started = Instant::now();
        let err = transport.poll(Duration::from_millis(100)).unwrap_err();
        assert!(matches!(err, Error::Io(ref e) if e.kind() == io::ErrorKind::WouldBlock));
        assert!(
            started.elapsed() >= Duration::from_millis(100),
            "must have waited"
        );
    }

    #[test]
    fn send_enforces_the_wire_bound_including_the_nul() {
        let (mut transport, peer) = FakePeer::new();
        // A body of exactly max bytes cannot fit with its NUL terminator:
        // the wire message would exceed the decoder's own bound.
        let max = transport.max_message_bytes();
        let err = transport.send(&vec![b'x'; max]).unwrap_err();
        assert!(matches!(err, Error::OversizedMessage(_)));
        // max - 1 bytes plus the NUL fits exactly.
        transport.send(&vec![b'x'; max - 1]).unwrap();
        drop(peer);
    }

    #[test]
    fn peer_close_surfaces_eof() {
        let (mut transport, peer) = FakePeer::new();
        drop(peer);
        // The close lands during the first poll; the error is due next.
        let _ = transport.poll(Duration::from_millis(2000));
        let err = transport.poll(Duration::from_millis(2000)).unwrap_err();
        assert!(matches!(err, Error::Io(ref e) if e.kind() == io::ErrorKind::UnexpectedEof));
    }

    #[test]
    fn messages_land_then_eof_surfaces() {
        // A peer that sends a message and immediately closes (chromium's
        // exit pattern) must deliver the message first and the EOF after,
        // never drop the message in favor of the error.
        let (mut transport, peer) = FakePeer::new();
        peer.write_body(br#"{"id":1}"#);
        drop(peer);
        let messages = transport.poll(Duration::from_millis(2000)).unwrap();
        assert_eq!(messages, vec![br#"{"id":1}"#.to_vec()]);
        let err = transport.poll(Duration::ZERO).unwrap_err();
        assert!(matches!(err, Error::Io(ref e) if e.kind() == io::ErrorKind::UnexpectedEof));
    }

    #[test]
    fn drop_closes_the_fds() {
        let (a, b) = socket_pair();
        {
            let transport = PipeTransport::new(a, b, 1024).unwrap();
            drop(transport);
        }
        // Both fds must now be closed (EBADF), not leaked.
        let err = unsafe { libc::fcntl(a, libc::F_GETFL) };
        assert_eq!(err, -1);
        let err2 = unsafe { libc::fcntl(b, libc::F_GETFL) };
        assert_eq!(err2, -1);
    }
}
