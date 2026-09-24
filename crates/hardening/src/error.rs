use std::error::Error;
use std::fmt;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HardeningError {
    InvalidConfiguration(&'static str),
    InvalidMetadata(String),
    Runtime(String),
    Adaptation(String),
    AssetStore(String),
    Control(String),
    Serialization(String),
    Invariant(String),
}

impl fmt::Display for HardeningError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidConfiguration(message) => {
                write!(f, "invalid hardening configuration: {message}")
            }
            Self::InvalidMetadata(message) => write!(f, "invalid metadata: {message}"),
            Self::Runtime(message) => write!(f, "runtime failure: {message}"),
            Self::Adaptation(message) => write!(f, "adaptation failure: {message}"),
            Self::AssetStore(message) => write!(f, "asset-store failure: {message}"),
            Self::Control(message) => write!(f, "control failure: {message}"),
            Self::Serialization(message) => write!(f, "serialization failure: {message}"),
            Self::Invariant(message) => write!(f, "hardening invariant failed: {message}"),
        }
    }
}

impl Error for HardeningError {}
