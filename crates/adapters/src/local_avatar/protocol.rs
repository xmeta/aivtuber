//! `local-avatar/1`: the adapter-private wire protocol between the Rust host
//! and the local renderer page (ADR-0003). It is deliberately not a
//! `schemas/` contract: it is a provider-specific DTO that stays behind the
//! adapter. Renderer-to-host frames are untrusted bounded data and never feed
//! authorization.

use aivtuber_domain::Viseme;
use serde::{Deserialize, Serialize};

/// Protocol identifier, documented and used in the page query string.
pub const PROTOCOL: &str = "local-avatar/1";
/// The `v` field of every frame.
pub const PROTOCOL_VERSION: u32 = 1;
/// Upper bound for one frame in either direction.
pub const MAX_FRAME_BYTES: usize = 4096;
const MAX_ID_LEN: usize = 64;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProtocolError {
    TooLarge,
    Malformed(String),
    UnsupportedVersion(u32),
    InvalidField(&'static str),
}

impl std::fmt::Display for ProtocolError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::TooLarge => write!(f, "frame exceeds {MAX_FRAME_BYTES} bytes"),
            Self::Malformed(message) => write!(f, "malformed frame: {message}"),
            Self::UnsupportedVersion(v) => write!(f, "unsupported protocol version {v}"),
            Self::InvalidField(field) => write!(f, "invalid field: {field}"),
        }
    }
}

impl std::error::Error for ProtocolError {}

/// Symbolic gaze targets only (same enum as `performance-asset.schema.json`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GazeTarget {
    Camera,
    Chat,
    Game,
    Speaker,
    Away,
}

impl GazeTarget {
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "camera" => Some(Self::Camera),
            "chat" => Some(Self::Chat),
            "game" => Some(Self::Game),
            "speaker" => Some(Self::Speaker),
            "away" => Some(Self::Away),
            _ => None,
        }
    }
}

/// Mirrors `AvatarPlaybackCommand` without `generation` / `at_ms`, which are
/// host-side scheduling state and never go on the wire.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case", deny_unknown_fields)]
pub enum WireCommand {
    Expression { preset: String, intensity: f64 },
    Gesture { preset: String, amplitude: f64 },
    Gaze { target: GazeTarget },
    Viseme { viseme: Viseme, weight: f64 },
    Reset {},
}

impl WireCommand {
    /// Short stable name used for the authorization action string.
    pub fn kind(&self) -> &'static str {
        match self {
            Self::Expression { .. } => "expression",
            Self::Gesture { .. } => "gesture",
            Self::Gaze { .. } => "gaze",
            Self::Viseme { .. } => "viseme",
            Self::Reset {} => "reset",
        }
    }

    pub fn validate(&self) -> Result<(), ProtocolError> {
        let (name, value, max) = match self {
            Self::Expression { preset, intensity } => {
                check_id(preset, "preset")?;
                ("intensity", *intensity, 1.0)
            }
            Self::Gesture { preset, amplitude } => {
                check_id(preset, "preset")?;
                ("amplitude", *amplitude, 2.0)
            }
            Self::Viseme { weight, .. } => ("weight", *weight, 1.0),
            Self::Gaze { .. } | Self::Reset {} => return Ok(()),
        };
        if value.is_finite() && (0.0..=max).contains(&value) {
            Ok(())
        } else {
            Err(ProtocolError::InvalidField(name))
        }
    }
}

/// host -> renderer.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HostFrame {
    pub v: u32,
    pub epoch: String,
    pub seq: u64,
    pub cmd: WireCommand,
}

impl HostFrame {
    pub fn new(epoch: &str, seq: u64, cmd: WireCommand) -> Self {
        Self {
            v: PROTOCOL_VERSION,
            epoch: epoch.to_owned(),
            seq,
            cmd,
        }
    }

    pub fn to_json(&self) -> String {
        serde_json::to_string(self).expect("host frame serializes")
    }

    /// Parses and validates a host frame (used by the mock renderer and
    /// fixtures; the host itself only produces these).
    pub fn parse(text: &str) -> Result<Self, ProtocolError> {
        let frame: Self = parse_bounded(text)?;
        check_version(frame.v)?;
        check_id(&frame.epoch, "epoch")?;
        if frame.seq == 0 {
            return Err(ProtocolError::InvalidField("seq"));
        }
        frame.cmd.validate()?;
        Ok(frame)
    }
}

/// Closed reason set so a hostile renderer cannot grow the health counters.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RejectReason {
    StaleEpoch,
    StaleSeq,
    UnknownPreset,
    InvalidArgument,
    NotReady,
    Other,
}

/// renderer -> host. `type` discriminates; unknown fields are refused.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum RendererFrame {
    Hello {
        v: u32,
        epoch: String,
        model_hash: String,
        profile: String,
    },
    /// Sent only after the first neutral frame of the epoch has rendered.
    Ready {
        epoch: String,
    },
    Ack {
        epoch: String,
        seq: u64,
    },
    Reject {
        epoch: String,
        seq: u64,
        reason: RejectReason,
    },
}

impl RendererFrame {
    pub fn to_json(&self) -> String {
        serde_json::to_string(self).expect("renderer frame serializes")
    }

    pub fn parse(text: &str) -> Result<Self, ProtocolError> {
        let frame: Self = parse_bounded(text)?;
        match &frame {
            Self::Hello {
                v,
                epoch,
                model_hash,
                profile,
            } => {
                check_version(*v)?;
                check_id(epoch, "epoch")?;
                check_id(profile, "profile")?;
                if model_hash.len() != 64 || !model_hash.bytes().all(|b| b.is_ascii_hexdigit()) {
                    return Err(ProtocolError::InvalidField("model_hash"));
                }
            }
            Self::Ready { epoch } => check_id(epoch, "epoch")?,
            Self::Ack { epoch, seq } | Self::Reject { epoch, seq, .. } => {
                check_id(epoch, "epoch")?;
                if *seq == 0 {
                    return Err(ProtocolError::InvalidField("seq"));
                }
            }
        }
        Ok(frame)
    }
}

fn parse_bounded<T: serde::de::DeserializeOwned>(text: &str) -> Result<T, ProtocolError> {
    if text.len() > MAX_FRAME_BYTES {
        return Err(ProtocolError::TooLarge);
    }
    serde_json::from_str(text).map_err(|error| ProtocolError::Malformed(error.to_string()))
}

fn check_version(v: u32) -> Result<(), ProtocolError> {
    if v == PROTOCOL_VERSION {
        Ok(())
    } else {
        Err(ProtocolError::UnsupportedVersion(v))
    }
}

/// Epoch, profile and preset ids: short, printable, no separators that could
/// matter to any downstream consumer.
fn check_id(value: &str, field: &'static str) -> Result<(), ProtocolError> {
    let ok = !value.is_empty()
        && value.len() <= MAX_ID_LEN
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'@'));
    if ok {
        Ok(())
    } else {
        Err(ProtocolError::InvalidField(field))
    }
}
