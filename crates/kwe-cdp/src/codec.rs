// SPDX-License-Identifier: Apache-2.0
//! ASCIIZ wire framing of the Chromium remote-debugging pipe.
//!
//! Chromium's `devtools_pipe_handler` (PipeReaderASCIIZ/PipeWriterASCIIZ)
//! speaks exactly one JSON document per message, each terminated by a single
//! NUL byte, in both directions. Messages are bounded (4 MiB by default) so
//! a hostile or wedged peer can never grow our buffers without limit; an
//! over-bound message poisons the decoder and the caller drops the
//! connection.

use crate::error::Error;

/// Default per-message bound (bytes of JSON body, not counting the NUL).
pub const DEFAULT_MAX_MESSAGE_BYTES: usize = 4 * 1024 * 1024;

/// Frame one message body for the wire: body + a single NUL terminator.
pub fn encode_message(body: &[u8]) -> Vec<u8> {
    let mut framed = Vec::with_capacity(body.len() + 1);
    framed.extend_from_slice(body);
    framed.push(0);
    framed
}

/// Incremental NUL-delimited decoder with a hard per-message bound.
///
/// Feed raw pipe bytes with [`Decoder::push`]; complete messages are
/// collected and fetched with [`Decoder::take_messages`]; an unterminated
/// tail stays buffered across pushes. Once the buffered tail can no longer
/// satisfy the bound, `push` returns [`Error::OversizedMessage`] and the
/// decoder is left poisoned — the connection must be dropped.
#[derive(Debug)]
pub struct Decoder {
    max_message_bytes: usize,
    buffer: Vec<u8>,
    scan_from: usize,
    messages: Vec<Vec<u8>>,
}

impl Decoder {
    pub fn new(max_message_bytes: usize) -> Self {
        Decoder {
            max_message_bytes,
            buffer: Vec::new(),
            scan_from: 0,
            messages: Vec::new(),
        }
    }

    /// Consume raw bytes; complete NUL-terminated messages become available
    /// via [`Decoder::take_messages`].
    pub fn push(&mut self, bytes: &[u8]) -> Result<(), Error> {
        if bytes.is_empty() {
            return Ok(());
        }
        self.buffer.extend_from_slice(bytes);
        while let Some(relative) = self.buffer[self.scan_from..].iter().position(|&b| b == 0) {
            let end = self.scan_from + relative;
            if end > self.max_message_bytes {
                return Err(Error::OversizedMessage(self.max_message_bytes));
            }
            let message = self.buffer[..end].to_vec();
            self.messages.push(message);
            self.buffer.drain(..=end);
            self.scan_from = 0;
        }
        // No terminator in the buffer: the remaining tail must still be able
        // to complete within the bound, or it can never succeed.
        if self.buffer.len() > self.max_message_bytes {
            return Err(Error::OversizedMessage(self.max_message_bytes));
        }
        self.scan_from = self.buffer.len();
        Ok(())
    }

    /// Take all complete messages, leaving the partial tail buffered.
    pub fn take_messages(&mut self) -> Vec<Vec<u8>> {
        std::mem::take(&mut self.messages)
    }

    /// Bytes of the unterminated tail currently buffered.
    pub fn buffered_bytes(&self) -> usize {
        self.buffer.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip_through_the_codec() {
        let mut decoder = Decoder::new(DEFAULT_MAX_MESSAGE_BYTES);
        let body = br#"{"id":1,"method":"Page.enable","params":{}}"#;
        decoder.push(&encode_message(body)).unwrap();
        let messages = decoder.take_messages();
        assert_eq!(messages, vec![body.to_vec()]);
        assert_eq!(decoder.buffered_bytes(), 0);
    }

    #[test]
    fn incremental_decode_at_every_byte_split() {
        let body = br#"{"method":"Page.screencastFrame","params":{"data":"AA=="}}"#;
        let framed = encode_message(body);
        for split in 0..framed.len() {
            let mut decoder = Decoder::new(DEFAULT_MAX_MESSAGE_BYTES);
            decoder.push(&framed[..split]).unwrap();
            assert_eq!(decoder.take_messages(), Vec::<Vec<u8>>::new());
            decoder.push(&framed[split..]).unwrap();
            assert_eq!(decoder.take_messages(), vec![body.to_vec()]);
        }
    }

    #[test]
    fn incremental_decode_in_one_byte_chunks() {
        let body = b"{\"id\":7}";
        let framed = encode_message(body);
        let mut decoder = Decoder::new(DEFAULT_MAX_MESSAGE_BYTES);
        for &byte in &framed {
            decoder.push(std::slice::from_ref(&byte)).unwrap();
        }
        assert_eq!(decoder.take_messages(), vec![body.to_vec()]);
    }

    #[test]
    fn multiple_messages_in_one_push() {
        let mut decoder = Decoder::new(DEFAULT_MAX_MESSAGE_BYTES);
        let a = br#"{"id":1}"#;
        let b = br#"{"id":2}"#;
        let c = br#"{"id":3}"#;
        let mut combined = encode_message(a);
        combined.extend_from_slice(&encode_message(b));
        combined.extend_from_slice(&encode_message(c));
        decoder.push(&combined).unwrap();
        assert_eq!(
            decoder.take_messages(),
            vec![a.to_vec(), b.to_vec(), c.to_vec()]
        );
    }

    #[test]
    fn partial_tail_stays_buffered_across_pushes() {
        let mut decoder = Decoder::new(DEFAULT_MAX_MESSAGE_BYTES);
        decoder.push(b"{\"id\":").unwrap();
        assert_eq!(decoder.take_messages(), Vec::<Vec<u8>>::new());
        assert_eq!(decoder.buffered_bytes(), 6);
        decoder.push(b"9}\0").unwrap();
        assert_eq!(decoder.take_messages(), vec![b"{\"id\":9}".to_vec()]);
        assert_eq!(decoder.buffered_bytes(), 0);
    }

    #[test]
    fn empty_message_is_accepted() {
        let mut decoder = Decoder::new(DEFAULT_MAX_MESSAGE_BYTES);
        decoder.push(b"\0").unwrap();
        assert_eq!(decoder.take_messages(), vec![Vec::<u8>::new()]);
    }

    #[test]
    fn oversized_tail_is_rejected() {
        let mut decoder = Decoder::new(16);
        let err = decoder.push(&[b'x'; 17]).unwrap_err();
        assert!(matches!(err, Error::OversizedMessage(16)));
    }

    #[test]
    fn oversized_complete_message_is_rejected() {
        let mut decoder = Decoder::new(16);
        // 17 body bytes followed by the NUL: the message itself exceeds the
        // bound even though the wire terminates it.
        let mut framed = vec![b'x'; 17];
        framed.push(0);
        let err = decoder.push(&framed).unwrap_err();
        assert!(matches!(err, Error::OversizedMessage(16)));
    }

    #[test]
    fn message_exactly_at_the_bound_passes() {
        let mut decoder = Decoder::new(16);
        let body = vec![b'x'; 16];
        decoder.push(&encode_message(&body)).unwrap();
        assert_eq!(decoder.take_messages(), vec![body]);
    }

    #[test]
    fn empty_push_is_a_noop() {
        let mut decoder = Decoder::new(DEFAULT_MAX_MESSAGE_BYTES);
        decoder.push(b"").unwrap();
        assert_eq!(decoder.take_messages(), Vec::<Vec<u8>>::new());
        assert_eq!(decoder.buffered_bytes(), 0);
    }
}
