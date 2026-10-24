//! Provider-neutral port for an external memory sidecar (#105).
//!
//! A sidecar is a memory service the runtime may delegate retention and
//! recall to: it keeps claims the project admitted, answers a scoped recall,
//! and can retire a claim that a later claim replaced. The port is what the
//! project owns; the service's wire format, its ranking, and its storage
//! model are the service's business and never become project authority.
//!
//! Three rules the port encodes, because a sidecar that ignored them would
//! make the comparison meaningless:
//!
//! * **Scope is the project's, not the service's.** A recall carries the
//!   scope it is restricted to, and the service answers inside it. The
//!   project still filters what it accepts, so a service that ignores the
//!   scope cannot widen it.
//! * **A handle is opaque.** The project never parses a service-side id; it
//!   only hands it back to retire a claim.
//! * **Retirement is not deletion by default.** A retired claim must stop
//!   being recalled; whether the service keeps it for audit is the service's
//!   choice, and the project does not depend on the difference.

use crate::interfaces::EngineError;
use serde::{Deserialize, Serialize};
use std::fmt;

/// Maximum bytes a claim handed to a sidecar may carry.
///
/// The port is a bounded boundary: a sidecar that accepts an unbounded claim
/// would make the service's storage, its index, and its response size
/// unbounded too. The delivery bound (`MAX_THINKING_TEXT_BYTES`) is
/// separate and smaller — a claim may be retained whole and still be too
/// large to hand to generation.
pub const MAX_SIDECAR_CLAIM_BYTES: usize = 64 * 1024;

/// Maximum bytes a recall query's text may carry.
pub const MAX_SIDECAR_QUERY_BYTES: usize = 4 * 1024;

/// Maximum number of items a recall may return.
///
/// The bound is the service's: a service that answered without one would
/// make the caller's own item budget meaningless. The caller still applies
/// its own delivery bound to whatever arrives.
pub const MAX_SIDECAR_RECALL_ITEMS: usize = 256;

/// A claim handed to a sidecar for retention.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SidecarClaim {
    /// Project-scoped identity of the claim (the source event id). Opaque to
    /// the service and stable across runs, so a recall can be mapped back to
    /// the claim it came from.
    pub claim_id: String,
    /// The scope key the claim belongs to — a pseudonym, never a raw actor
    /// id. `None` means the claim carries no actor, which is a fact about
    /// the claim, not an invitation to share it.
    pub actor: Option<String>,
    /// A topic label used for retrieval. It is a ranking signal, never a
    /// filter: a claim whose topic differs from the request's may still be
    /// the one the request needs.
    pub topic: Option<String>,
    /// The claim text, verbatim.
    pub text: String,
    /// When the claim was recorded (logical record time, milliseconds).
    pub recorded_at_ms: u64,
    /// When the claim's content occurred (event time, milliseconds). A
    /// sidecar that keeps event time can order by what happened; one that
    /// does not can only order by when it was told.
    pub event_at_ms: u64,
}

impl SidecarClaim {
    /// Refuse a claim the port cannot carry: an empty or oversized text, a
    /// missing identity, or a scope key that is not a usable token.
    pub fn validate(&self) -> Result<(), EngineError> {
        if self.claim_id.trim().is_empty() {
            return Err(EngineError::new(
                crate::interfaces::EngineErrorKind::InvalidRequest,
                "a sidecar claim needs a claim id",
            ));
        }
        if self.text.is_empty() {
            return Err(EngineError::new(
                crate::interfaces::EngineErrorKind::InvalidRequest,
                "a sidecar claim needs text",
            ));
        }
        if self.text.len() > MAX_SIDECAR_CLAIM_BYTES {
            return Err(EngineError::new(
                crate::interfaces::EngineErrorKind::InvalidRequest,
                format!(
                    "a sidecar claim may carry at most {MAX_SIDECAR_CLAIM_BYTES} bytes, got {}",
                    self.text.len()
                ),
            ));
        }
        Ok(())
    }
}

/// A recall request against one scope.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SidecarRecallQuery {
    /// The scope the recall is restricted to. `None` asks for the scope that
    /// carries no actor, not for every scope.
    pub actor: Option<String>,
    /// The retrieval text: the request's own signal, never an authorization.
    pub query: String,
    /// How many items the caller will use at most.
    pub limit: usize,
    /// The read timestamp (milliseconds) recency is anchored on.
    pub now_ms: u64,
}

impl SidecarRecallQuery {
    /// Refuse a query the port cannot carry.
    pub fn validate(&self) -> Result<(), EngineError> {
        if self.query.len() > MAX_SIDECAR_QUERY_BYTES {
            return Err(EngineError::new(
                crate::interfaces::EngineErrorKind::InvalidRequest,
                format!(
                    "a sidecar query may carry at most {MAX_SIDECAR_QUERY_BYTES} bytes, got {}",
                    self.query.len()
                ),
            ));
        }
        if self.limit == 0 || self.limit > MAX_SIDECAR_RECALL_ITEMS {
            return Err(EngineError::new(
                crate::interfaces::EngineErrorKind::InvalidRequest,
                format!(
                    "a sidecar recall limit must be 1..={MAX_SIDECAR_RECALL_ITEMS}, got {}",
                    self.limit
                ),
            ));
        }
        Ok(())
    }
}

/// One recalled item.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SidecarRecallItem {
    /// The project-scoped identity of the claim this item carries.
    pub claim_id: String,
    /// The text the service would hand to generation.
    pub text: String,
    /// The scope key the item belongs to, when the service kept one.
    pub actor: Option<String>,
    /// The topic label the service kept, when it kept one.
    pub topic: Option<String>,
    /// The event time the service recorded, when it kept one.
    pub event_at_ms: Option<u64>,
}

/// A service-side handle for a retained claim.
///
/// The project never interprets it: it is opaque, it is only handed back to
/// retire the claim it names.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SidecarHandle(String);

impl SidecarHandle {
    /// Wrap a service-side identifier.
    pub fn new(id: impl Into<String>) -> Self {
        Self(id.into())
    }

    /// The identifier as the service spelled it.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for SidecarHandle {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// A bounded external memory service.
///
/// The service is reached through scopes: an isolated namespace the project
/// opens before it writes and closes when it is done, so one measurement
/// cannot see another measurement's claims.
pub trait MemorySidecar: Send + Sync {
    /// Open an isolated scope. Opening an existing scope is the service's
    /// business; the project only asks for one it can write into.
    fn open_scope(&self, scope: &str) -> Result<(), EngineError>;

    /// Retain a claim in `scope` and return the handle that names it.
    fn retain(&self, scope: &str, claim: &SidecarClaim) -> Result<SidecarHandle, EngineError>;

    /// Answer a scoped recall.
    fn recall(
        &self,
        scope: &str,
        query: &SidecarRecallQuery,
    ) -> Result<Vec<SidecarRecallItem>, EngineError>;

    /// Retire a retained claim: it must stop being recalled. Whether the
    /// service keeps it for audit is the service's choice.
    fn retire(&self, scope: &str, handle: &SidecarHandle) -> Result<(), EngineError>;

    /// Drop the scope and everything in it.
    fn close_scope(&self, scope: &str) -> Result<(), EngineError>;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::interfaces::EngineErrorKind;

    fn claim() -> SidecarClaim {
        SidecarClaim {
            claim_id: "evt-1".to_owned(),
            actor: Some("actor:memory-eval-v1:abc".to_owned()),
            topic: Some("game-x".to_owned()),
            text: "viewer enjoyed the game x ending".to_owned(),
            recorded_at_ms: 2000,
            event_at_ms: 2000,
        }
    }

    #[test]
    fn claim_validation_refuses_empty_identity_and_oversized_text() {
        let mut invalid = claim();
        invalid.claim_id = "  ".to_owned();
        assert_eq!(
            invalid.validate().expect_err("empty id").kind,
            EngineErrorKind::InvalidRequest
        );

        let mut invalid = claim();
        invalid.text = String::new();
        assert_eq!(
            invalid.validate().expect_err("empty text").kind,
            EngineErrorKind::InvalidRequest
        );

        let mut invalid = claim();
        invalid.text = "x".repeat(MAX_SIDECAR_CLAIM_BYTES + 1);
        assert_eq!(
            invalid.validate().expect_err("oversized text").kind,
            EngineErrorKind::InvalidRequest
        );

        claim().validate().expect("valid claim");
    }

    #[test]
    fn recall_query_validation_bounds_limit_and_text() {
        let query = SidecarRecallQuery {
            actor: None,
            query: "game x".to_owned(),
            limit: 12,
            now_ms: 3000,
        };
        query.validate().expect("valid query");

        let mut invalid = query.clone();
        invalid.limit = 0;
        assert_eq!(
            invalid.validate().expect_err("zero limit").kind,
            EngineErrorKind::InvalidRequest
        );

        let mut invalid = query.clone();
        invalid.limit = MAX_SIDECAR_RECALL_ITEMS + 1;
        assert_eq!(
            invalid.validate().expect_err("oversized limit").kind,
            EngineErrorKind::InvalidRequest
        );

        let mut invalid = query;
        invalid.query = "q".repeat(MAX_SIDECAR_QUERY_BYTES + 1);
        assert_eq!(
            invalid.validate().expect_err("oversized query").kind,
            EngineErrorKind::InvalidRequest
        );
    }

    #[test]
    fn handle_keeps_the_service_identifier_verbatim() {
        let handle = SidecarHandle::new("25b779ed-64bd-436c-9a5c-9c272d08a1db");
        assert_eq!(handle.as_str(), "25b779ed-64bd-436c-9a5c-9c272d08a1db");
        assert_eq!(handle.to_string(), "25b779ed-64bd-436c-9a5c-9c272d08a1db");
    }
}
