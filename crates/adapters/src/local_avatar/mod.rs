//! Local avatar adapter: a loopback HTTP + WebSocket host for the bundled
//! renderer page (ADR-0003, issue #219).
//!
//! The renderer page is the WebSocket *client*. The Rust side owns ordering,
//! freshness and authorization:
//!
//! * the renderer mints the epoch (in `hello`), the host mints a strictly
//!   increasing `seq` per epoch starting at 1 (the first frame of every epoch
//!   is `reset`, sent only after `ready`);
//! * the outbound queue is bounded and is never replayed across epochs;
//! * nothing here can block or fail the caller: [`LocalAvatarHandle::enqueue`]
//!   only takes a short mutex and either queues or drops-and-counts.

mod protocol;
mod server;

pub use protocol::*;
pub use server::LocalAvatarServer;

use std::collections::{BTreeMap, VecDeque};
use std::fmt;
use std::path::PathBuf;
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

/// Max commands sent but not yet acked/rejected. Beyond this the renderer is
/// considered unresponsive (`Degraded`) and sending pauses.
pub(crate) const MAX_IN_FLIGHT: usize = 64;

#[derive(Clone)]
pub struct LocalAvatarConfig {
    /// TCP port on 127.0.0.1. `0` picks an ephemeral port (tests).
    pub port: u16,
    /// Directory of the static renderer bundle; `None` serves 404 for pages.
    pub page_dir: Option<PathBuf>,
    /// Bounded outbound queue capacity; when full, commands are dropped.
    pub queue_capacity: usize,
    /// A queued command older than this when it would be sent is dropped as
    /// expired (the dispatch window has passed).
    pub max_command_age: Duration,
}

impl Default for LocalAvatarConfig {
    fn default() -> Self {
        Self {
            port: 8765,
            page_dir: None,
            queue_capacity: 64,
            max_command_age: Duration::from_millis(250),
        }
    }
}

impl fmt::Debug for LocalAvatarConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("LocalAvatarConfig")
            .field("port", &self.port)
            .field("page_dir", &self.page_dir)
            .field("queue_capacity", &self.queue_capacity)
            .field("max_command_age", &self.max_command_age)
            .finish()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LocalAvatarState {
    /// No renderer connection.
    Disconnected,
    /// Connected; waiting for `hello` and `ready`.
    Handshaking,
    Ready,
    /// Ready but the renderer has too many unanswered commands.
    Degraded,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum DropReason {
    /// Outbound queue was full.
    QueueFull,
    /// The command's dispatch window had passed before it could be sent.
    Expired,
    /// No renderer connection.
    NotReady,
    /// Queued for a previous epoch/connection; never replayed.
    StaleEpoch,
    /// The command has no wire form (for example an unknown gaze target).
    Unmapped,
}

/// Bounded health snapshot: closed enums for every counter key.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LocalAvatarHealth {
    pub state: LocalAvatarState,
    pub sent: u64,
    pub acked: u64,
    pub rejected: BTreeMap<RejectReason, u64>,
    pub dropped: BTreeMap<DropReason, u64>,
    /// Renderer connections after the first one.
    pub reconnects: u64,
    /// Frames from the renderer that were malformed or out of protocol.
    pub protocol_errors: u64,
    /// Acks/rejects for an unknown epoch or seq.
    pub stray_acks: u64,
}

impl LocalAvatarHealth {
    fn new() -> Self {
        Self {
            state: LocalAvatarState::Disconnected,
            sent: 0,
            acked: 0,
            rejected: BTreeMap::new(),
            dropped: BTreeMap::new(),
            reconnects: 0,
            protocol_errors: 0,
            stray_acks: 0,
        }
    }

    pub fn dropped_count(&self, reason: DropReason) -> u64 {
        self.dropped.get(&reason).copied().unwrap_or(0)
    }

    pub fn rejected_count(&self, reason: RejectReason) -> u64 {
        self.rejected.get(&reason).copied().unwrap_or(0)
    }
}

pub(crate) struct Queued {
    pub cmd: WireCommand,
    pub enqueued: Instant,
}

pub(crate) struct Inner {
    pub queue: VecDeque<Queued>,
    pub health: LocalAvatarHealth,
    pub session_active: bool,
    pub sessions_seen: u64,
}

impl Inner {
    pub fn drop_one(&mut self, reason: DropReason) {
        *self.health.dropped.entry(reason).or_insert(0) += 1;
    }

    /// Drops everything queued, e.g. at an epoch boundary.
    pub fn purge(&mut self, reason: DropReason) {
        let n = self.queue.len() as u64;
        self.queue.clear();
        if n > 0 {
            *self.health.dropped.entry(reason).or_insert(0) += n;
        }
    }
}

pub(crate) struct Shared {
    pub inner: Mutex<Inner>,
    /// Per-launch secret. Never logged; see the `Debug` impls.
    pub token: String,
    /// `http://127.0.0.1:<port>`: the only acceptable `Origin`.
    pub origin: String,
    /// `127.0.0.1:<port>`: the only acceptable `Host` (DNS-rebinding guard).
    pub host: String,
    pub config: LocalAvatarConfig,
    pub shutdown: AtomicBool,
}

impl Shared {
    pub fn lock(&self) -> MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub fn new(config: LocalAvatarConfig, token: String, port: u16) -> Self {
        Self {
            inner: Mutex::new(Inner {
                queue: VecDeque::new(),
                health: LocalAvatarHealth::new(),
                session_active: false,
                sessions_seen: 0,
            }),
            token,
            origin: format!("http://127.0.0.1:{port}"),
            host: format!("127.0.0.1:{port}"),
            config,
            shutdown: AtomicBool::new(false),
        }
    }
}

/// Cloneable, non-blocking producer-side view of the adapter.
#[derive(Clone)]
pub struct LocalAvatarHandle {
    pub(crate) shared: Arc<Shared>,
}

impl fmt::Debug for LocalAvatarHandle {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("LocalAvatarHandle")
            .field("origin", &self.shared.origin)
            .field("token", &"[REDACTED]")
            .finish()
    }
}

impl LocalAvatarHandle {
    /// Never blocks on the renderer or the network: queue or drop-and-count.
    pub fn enqueue(&self, cmd: WireCommand) -> Result<(), DropReason> {
        let mut inner = self.shared.lock();
        let reason = if inner.health.state == LocalAvatarState::Disconnected {
            Some(DropReason::NotReady)
        } else if inner.queue.len() >= self.shared.config.queue_capacity {
            Some(DropReason::QueueFull)
        } else {
            None
        };
        if let Some(reason) = reason {
            inner.drop_one(reason);
            return Err(reason);
        }
        inner.queue.push_back(Queued {
            cmd,
            enqueued: Instant::now(),
        });
        Ok(())
    }

    /// Counts a command that could not be mapped to a wire command.
    pub fn record_drop(&self, reason: DropReason) {
        self.shared.lock().drop_one(reason);
    }

    pub fn health(&self) -> LocalAvatarHealth {
        self.shared.lock().health.clone()
    }

    pub fn origin(&self) -> &str {
        &self.shared.origin
    }
}
