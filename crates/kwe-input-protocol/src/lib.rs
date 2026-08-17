// SPDX-License-Identifier: Apache-2.0
//! Bounded normalized-input messages between the daemon and an isolated
//! renderer worker.
//!
//! This wire format and implementation are original. Upstream projects in
//! `THIRD_PARTY.yml` informed only the process-boundary goal.

use serde::{Deserialize, Serialize};
use thiserror::Error;

pub const VERSION: u32 = 1;
pub const MAX_MESSAGE_BYTES: usize = 4096;
pub const MAX_AUDIO_BANDS: usize = 64;
pub const COORDINATE_MAX: u16 = u16::MAX;

#[derive(Debug, Clone, Copy, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum PointerPhase {
    Enter,
    Move,
    Leave,
    Down,
    Up,
}

#[derive(Debug, Clone, Copy, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum PointerButton {
    Primary,
    Secondary,
    Middle,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct PointerMessage {
    pub version: u32,
    #[serde(rename = "type")]
    pub message_type: String,
    pub sequence: u64,
    pub phase: PointerPhase,
    pub x: u16,
    pub y: u16,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub button: Option<PointerButton>,
}

impl PointerMessage {
    pub fn from_normalized(
        sequence: u64,
        phase: PointerPhase,
        x: f64,
        y: f64,
    ) -> Result<Self, InputProtocolError> {
        if sequence == 0 {
            return Err(InputProtocolError::InvalidSequence);
        }
        Ok(Self {
            version: VERSION,
            message_type: "pointer_position".to_string(),
            sequence,
            phase,
            x: quantize_coordinate(x)?,
            y: quantize_coordinate(y)?,
            button: None,
        })
    }

    pub fn button_event(
        sequence: u64,
        phase: PointerPhase,
        button: PointerButton,
        x: f64,
        y: f64,
    ) -> Result<Self, InputProtocolError> {
        if !matches!(phase, PointerPhase::Down | PointerPhase::Up) {
            return Err(InputProtocolError::InvalidButtonPhase);
        }
        let mut message = Self::from_normalized(sequence, phase, x, y)?;
        message.button = Some(button);
        Ok(message)
    }

    pub fn normalized_x(&self) -> f64 {
        f64::from(self.x) / f64::from(COORDINATE_MAX)
    }

    pub fn normalized_y(&self) -> f64 {
        f64::from(self.y) / f64::from(COORDINATE_MAX)
    }

    fn validate(&self) -> Result<(), InputProtocolError> {
        if self.version != VERSION {
            return Err(InputProtocolError::UnsupportedVersion(self.version));
        }
        if self.message_type != "pointer_position" {
            return Err(InputProtocolError::UnexpectedMessageType);
        }
        if self.sequence == 0 {
            return Err(InputProtocolError::InvalidSequence);
        }
        if matches!(self.phase, PointerPhase::Down | PointerPhase::Up) != self.button.is_some() {
            return Err(InputProtocolError::InvalidButtonPhase);
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct InputAck {
    pub version: u32,
    #[serde(rename = "type")]
    pub message_type: String,
    pub sequence: u64,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct AudioFrame {
    pub version: u32,
    #[serde(rename = "type")]
    pub message_type: String,
    pub sequence: u64,
    pub left: Vec<f32>,
    pub right: Vec<f32>,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct MediaState {
    pub version: u32,
    #[serde(rename = "type")]
    pub message_type: String,
    pub sequence: u64,
    pub playback: String,
    pub title: Option<String>,
    pub artist: Option<String>,
    pub album: Option<String>,
    pub position_seconds: Option<f64>,
    pub duration_seconds: Option<f64>,
}

impl MediaState {
    pub fn new(
        sequence: u64,
        playback: &str,
        title: Option<String>,
        artist: Option<String>,
        album: Option<String>,
        position_seconds: Option<f64>,
        duration_seconds: Option<f64>,
    ) -> Result<Self, InputProtocolError> {
        if sequence == 0 || !matches!(playback, "playing" | "paused" | "stopped") {
            return Err(InputProtocolError::InvalidMediaState);
        }
        for value in [position_seconds, duration_seconds].into_iter().flatten() {
            if !value.is_finite() || !(0.0..=86_400.0).contains(&value) {
                return Err(InputProtocolError::InvalidMediaState);
            }
        }
        Ok(Self {
            version: VERSION,
            message_type: "media_state".into(),
            sequence,
            playback: playback.into(),
            title: truncate(title),
            artist: truncate(artist),
            album: truncate(album),
            position_seconds,
            duration_seconds,
        })
    }
}

fn truncate(value: Option<String>) -> Option<String> {
    value.map(|text| text.chars().take(512).collect())
}

impl AudioFrame {
    pub fn new(sequence: u64, left: Vec<f32>, right: Vec<f32>) -> Result<Self, InputProtocolError> {
        if sequence == 0 || left.len() != right.len() || !matches!(left.len(), 16 | 32 | 64) {
            return Err(InputProtocolError::InvalidAudioBands);
        }
        if left
            .iter()
            .chain(right.iter())
            .any(|value| !value.is_finite() || !(0.0..=1.0).contains(value))
        {
            return Err(InputProtocolError::InvalidAudioValue);
        }
        Ok(Self {
            version: VERSION,
            message_type: "audio_bands".into(),
            sequence,
            left,
            right,
        })
    }
}

impl InputAck {
    pub fn new(sequence: u64) -> Result<Self, InputProtocolError> {
        if sequence == 0 {
            return Err(InputProtocolError::InvalidSequence);
        }
        Ok(Self {
            version: VERSION,
            message_type: "input_ack".to_string(),
            sequence,
        })
    }

    fn validate(&self) -> Result<(), InputProtocolError> {
        if self.version != VERSION {
            return Err(InputProtocolError::UnsupportedVersion(self.version));
        }
        if self.message_type != "input_ack" {
            return Err(InputProtocolError::UnexpectedMessageType);
        }
        if self.sequence == 0 {
            return Err(InputProtocolError::InvalidSequence);
        }
        Ok(())
    }
}

#[derive(Debug, Error)]
pub enum InputProtocolError {
    #[error("input message exceeds {MAX_MESSAGE_BYTES} bytes")]
    MessageTooLarge,
    #[error("input message is empty or contains an embedded newline")]
    InvalidFraming,
    #[error("unsupported input protocol version {0}")]
    UnsupportedVersion(u32),
    #[error("unexpected input message type")]
    UnexpectedMessageType,
    #[error("input sequence must be non-zero")]
    InvalidSequence,
    #[error("normalized pointer coordinate must be finite and in 0..=1")]
    InvalidCoordinate,
    #[error("audio frame must contain matching 16, 32, or 64 bands")]
    InvalidAudioBands,
    #[error("audio values must be finite and in 0..=1")]
    InvalidAudioValue,
    #[error("pointer button is required only for down/up phases")]
    InvalidButtonPhase,
    #[error("invalid media playback state or timeline")]
    InvalidMediaState,
    #[error("invalid input JSON: {0}")]
    InvalidJson(#[from] serde_json::Error),
}

pub fn encode_pointer_line(message: &PointerMessage) -> Result<Vec<u8>, InputProtocolError> {
    message.validate()?;
    encode_line(message)
}

pub fn decode_pointer_line(bytes: &[u8]) -> Result<PointerMessage, InputProtocolError> {
    let payload = validate_framing(bytes)?;
    let message: PointerMessage = serde_json::from_slice(payload)?;
    message.validate()?;
    Ok(message)
}

pub fn encode_ack_line(ack: &InputAck) -> Result<Vec<u8>, InputProtocolError> {
    ack.validate()?;
    encode_line(ack)
}

pub fn decode_ack_line(bytes: &[u8]) -> Result<InputAck, InputProtocolError> {
    let payload = validate_framing(bytes)?;
    let ack: InputAck = serde_json::from_slice(payload)?;
    ack.validate()?;
    Ok(ack)
}

pub fn encode_audio_frame(frame: &AudioFrame) -> Result<Vec<u8>, InputProtocolError> {
    if frame.version != VERSION || frame.message_type != "audio_bands" {
        return Err(InputProtocolError::UnexpectedMessageType);
    }
    AudioFrame::new(frame.sequence, frame.left.clone(), frame.right.clone())?;
    encode_line(frame)
}

pub fn decode_audio_frame(bytes: &[u8]) -> Result<AudioFrame, InputProtocolError> {
    let payload = validate_framing(bytes)?;
    let frame: AudioFrame = serde_json::from_slice(payload)?;
    if frame.version != VERSION || frame.message_type != "audio_bands" {
        return Err(InputProtocolError::UnexpectedMessageType);
    }
    AudioFrame::new(frame.sequence, frame.left.clone(), frame.right.clone())
}

pub fn encode_media_state(state: &MediaState) -> Result<Vec<u8>, InputProtocolError> {
    MediaState::new(
        state.sequence,
        &state.playback,
        state.title.clone(),
        state.artist.clone(),
        state.album.clone(),
        state.position_seconds,
        state.duration_seconds,
    )?;
    encode_line(state)
}

pub fn decode_media_state(bytes: &[u8]) -> Result<MediaState, InputProtocolError> {
    let payload = validate_framing(bytes)?;
    let state: MediaState = serde_json::from_slice(payload)?;
    if state.version != VERSION || state.message_type != "media_state" {
        return Err(InputProtocolError::UnexpectedMessageType);
    }
    MediaState::new(
        state.sequence,
        &state.playback,
        state.title,
        state.artist,
        state.album,
        state.position_seconds,
        state.duration_seconds,
    )
}

fn encode_line(value: &impl Serialize) -> Result<Vec<u8>, InputProtocolError> {
    let mut bytes = serde_json::to_vec(value)?;
    bytes.push(b'\n');
    if bytes.len() > MAX_MESSAGE_BYTES {
        return Err(InputProtocolError::MessageTooLarge);
    }
    Ok(bytes)
}

fn validate_framing(bytes: &[u8]) -> Result<&[u8], InputProtocolError> {
    if bytes.is_empty() || bytes.len() > MAX_MESSAGE_BYTES {
        return Err(if bytes.len() > MAX_MESSAGE_BYTES {
            InputProtocolError::MessageTooLarge
        } else {
            InputProtocolError::InvalidFraming
        });
    }
    let payload = bytes.strip_suffix(b"\n").unwrap_or(bytes);
    if payload.is_empty() || payload.contains(&b'\n') || payload.contains(&b'\r') {
        return Err(InputProtocolError::InvalidFraming);
    }
    Ok(payload)
}

fn quantize_coordinate(value: f64) -> Result<u16, InputProtocolError> {
    if !value.is_finite() || !(0.0..=1.0).contains(&value) {
        return Err(InputProtocolError::InvalidCoordinate);
    }
    Ok((value * f64::from(COORDINATE_MAX)).round() as u16)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pointer_round_trip_is_bounded_and_deterministic() {
        let message = PointerMessage::from_normalized(7, PointerPhase::Move, 0.5, 1.0).unwrap();
        assert_eq!(message.x, 32_768);
        assert_eq!(message.y, COORDINATE_MAX);
        let line = encode_pointer_line(&message).unwrap();
        assert!(line.len() <= MAX_MESSAGE_BYTES);
        assert_eq!(decode_pointer_line(&line).unwrap(), message);
    }

    #[test]
    fn rejects_non_finite_and_out_of_range_coordinates() {
        for value in [f64::NAN, f64::INFINITY, -0.01, 1.01] {
            assert!(PointerMessage::from_normalized(1, PointerPhase::Move, value, 0.5).is_err());
        }
    }

    #[test]
    fn ack_round_trip_rejects_wrong_type_and_oversize() {
        let ack = InputAck::new(9).unwrap();
        assert_eq!(
            decode_ack_line(&encode_ack_line(&ack).unwrap()).unwrap(),
            ack
        );
        assert!(
            decode_ack_line(br#"{"version":1,"type":"pointer_position","sequence":9}"#).is_err()
        );
        assert!(decode_ack_line(&vec![b'x'; MAX_MESSAGE_BYTES + 1]).is_err());
    }

    #[test]
    fn audio_round_trip_is_bounded_and_stereo() {
        let frame = AudioFrame::new(4, vec![0.25; 32], vec![0.75; 32]).unwrap();
        let line = encode_audio_frame(&frame).unwrap();
        assert_eq!(decode_audio_frame(&line).unwrap(), frame);
        assert!(AudioFrame::new(1, vec![0.0; 8], vec![0.0; 8]).is_err());
        assert!(AudioFrame::new(1, vec![1.2; 16], vec![0.0; 16]).is_err());
    }

    #[test]
    fn pointer_button_events_are_explicit_and_bounded() {
        let message =
            PointerMessage::button_event(8, PointerPhase::Down, PointerButton::Primary, 0.5, 0.5)
                .unwrap();
        assert_eq!(
            decode_pointer_line(&encode_pointer_line(&message).unwrap()).unwrap(),
            message
        );
        assert!(
            PointerMessage::button_event(8, PointerPhase::Move, PointerButton::Primary, 0.5, 0.5)
                .is_err()
        );
    }

    #[test]
    fn media_state_round_trip_bounds_timeline() {
        let state = MediaState::new(
            2,
            "playing",
            Some("Track".into()),
            None,
            None,
            Some(1.5),
            Some(120.0),
        )
        .unwrap();
        assert_eq!(
            decode_media_state(&encode_media_state(&state).unwrap()).unwrap(),
            state
        );
        assert!(MediaState::new(2, "playing", None, None, None, Some(-1.0), None).is_err());
    }
}
