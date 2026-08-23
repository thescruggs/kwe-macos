// SPDX-License-Identifier: GPL-3.0-or-later
//! Original, minimal, bounded CDP client for the Chromium remote-debugging
//! pipe (`--remote-debugging-pipe`).
//!
//! Chromium frames DevTools Protocol traffic as one JSON document per NUL
//! byte over fixed file descriptors (read fd 3, write fd 4 in the browser;
//! the client owns the opposite pipe ends). This crate implements that
//! ASCIIZ framing, request/response id correlation, a bounded event queue
//! and request timeouts on top of nonblocking pipes pumped with `poll(2)` —
//! no async runtime, no threads inside the library. The renderer worker
//! (M2b) drives screencast capture through this client from its own bounded
//! loop; everything here is bounded: 4 MiB per message, 64 in-flight
//! requests, 64 queued events (drop-oldest), 4 MiB write backlog, 64 KiB
//! read budget per poll.
//!
//! The wire contract is pinned empirically in `docs/BETA_M2.md` against
//! Chromium 151.0.7922.137 (`--headless=new`); nothing in this crate
//! requires a live browser — unit tests run against socketpair peers.

pub mod codec;
pub mod connection;
pub mod error;
pub mod transport;

mod client;

pub use client::Client;
pub use codec::{DEFAULT_MAX_MESSAGE_BYTES, Decoder};
pub use connection::{
    Connection, DEFAULT_EVENT_QUEUE_LIMIT, Event, MAX_IN_FLIGHT_REQUESTS, Response,
};
pub use error::Error;
pub use transport::{DEFAULT_READ_BUDGET_BYTES, MAX_WRITE_BACKLOG_BYTES, PipeTransport};

/// Test-only helper shared by the module test suites: a fresh socketpair for
/// fake peers, so traffic can be scripted without a browser. Two socketpairs
/// make up one fake chromium (one channel per pipe direction).
#[cfg(test)]
pub(crate) mod testutil {
    use std::os::fd::RawFd;

    pub(crate) fn socket_pair() -> (RawFd, RawFd) {
        let mut fds = [0 as RawFd; 2];
        let rc = unsafe {
            libc::socketpair(
                libc::AF_UNIX,
                libc::SOCK_STREAM | libc::SOCK_CLOEXEC,
                0,
                fds.as_mut_ptr(),
            )
        };
        assert_eq!(rc, 0, "socketpair failed");
        (fds[0], fds[1])
    }
}
