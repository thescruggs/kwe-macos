// SPDX-License-Identifier: GPL-3.0-or-later
//! Error surface of the CDP client.

use std::io;
use std::time::Duration;

use thiserror::Error;

/// Errors produced by the kwe-cdp client.
#[derive(Debug, Error)]
pub enum Error {
    /// A request did not receive its response within the timeout.
    #[error("CDP request timed out after {0:?}")]
    Timeout(Duration),
    /// A message from the peer could not be parsed as a CDP envelope.
    #[error("invalid CDP message: {0}")]
    ParseError(String),
    /// A message exceeded the per-message bound (default 4 MiB).
    #[error("CDP message exceeds the {0}-byte bound")]
    OversizedMessage(usize),
    /// The pipe failed, was closed by the peer, or the peer stopped
    /// draining our writes within the deadline.
    #[error("CDP pipe: {0}")]
    Io(#[from] io::Error),
}
