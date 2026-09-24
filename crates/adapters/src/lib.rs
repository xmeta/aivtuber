#![forbid(unsafe_code)]

//! Real stream/avatar adapter boundaries.
//!
//! Provider-specific websocket wire contracts live here rather than in the
//! provider-neutral domain. Connections fail closed, actions are never queued
//! for replay after a disconnect, and secrets use redacted debug output.

mod audio;
mod obs;
mod transport;
mod vts;

pub use audio::*;
pub use obs::*;
pub use transport::*;
pub use vts::*;

use serde_json::Value;
use std::fmt;
use std::sync::Arc;
use std::time::Instant;

#[derive(Clone, PartialEq, Eq)]
pub struct SecretString(String);

impl SecretString {
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    pub(crate) fn expose(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for SecretString {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("SecretString([REDACTED])")
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReconnectPolicy {
    pub initial_delay_ms: u64,
    pub max_delay_ms: u64,
}

impl Default for ReconnectPolicy {
    fn default() -> Self {
        Self {
            initial_delay_ms: 250,
            max_delay_ms: 5_000,
        }
    }
}

impl ReconnectPolicy {
    pub fn delay_ms(self, failures: u32) -> u64 {
        let exponent = failures.saturating_sub(1).min(20);
        self.initial_delay_ms
            .saturating_mul(1_u64 << exponent)
            .min(self.max_delay_ms)
    }
}

pub trait AdapterClock: Send + Sync {
    fn now_ms(&self) -> u64;
}

#[derive(Debug)]
pub struct StdAdapterClock {
    started: Instant,
}

impl Default for StdAdapterClock {
    fn default() -> Self {
        Self {
            started: Instant::now(),
        }
    }
}

impl AdapterClock for StdAdapterClock {
    fn now_ms(&self) -> u64 {
        self.started.elapsed().as_millis().min(u128::from(u64::MAX)) as u64
    }
}

pub fn default_clock() -> Arc<dyn AdapterClock> {
    Arc::new(StdAdapterClock::default())
}

#[derive(Debug, Clone, PartialEq)]
pub struct AdapterEvent {
    pub source: &'static str,
    pub event_type: String,
    pub data: Value,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct ReconnectState {
    failures: u32,
    next_retry_at_ms: u64,
}

impl ReconnectState {
    pub(crate) fn can_attempt(self, now_ms: u64) -> bool {
        now_ms >= self.next_retry_at_ms
    }

    pub(crate) fn connected(&mut self) {
        self.failures = 0;
        self.next_retry_at_ms = 0;
    }

    pub(crate) fn failed(&mut self, now_ms: u64, policy: ReconnectPolicy) {
        self.failures = self.failures.saturating_add(1);
        self.next_retry_at_ms = now_ms.saturating_add(policy.delay_ms(self.failures));
    }

    pub(crate) fn next_retry_at_ms(self) -> u64 {
        self.next_retry_at_ms
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn secret_debug_is_redacted() {
        let secret = SecretString::new("super-secret");
        let debug = format!("{secret:?}");
        assert!(!debug.contains("super-secret"));
        assert!(debug.contains("REDACTED"));
    }

    #[test]
    fn reconnect_backoff_is_bounded() {
        let policy = ReconnectPolicy {
            initial_delay_ms: 100,
            max_delay_ms: 800,
        };
        assert_eq!(policy.delay_ms(1), 100);
        assert_eq!(policy.delay_ms(2), 200);
        assert_eq!(policy.delay_ms(4), 800);
        assert_eq!(policy.delay_ms(10), 800);
    }
}
