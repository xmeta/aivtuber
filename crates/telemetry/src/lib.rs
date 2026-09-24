#![forbid(unsafe_code)]

//! Telemetry vocabulary shared by benchmarks and runtime instrumentation.

/// Initial metric groups required by the architecture.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MetricGroup {
    Latency,
    Routing,
    Cache,
    Generation,
    Fallback,
    Security,
}

/// Security audit categories contain decisions, never credentials.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuditCategory {
    Ingress,
    Authorization,
    Output,
    Memory,
}

/// Minimal structured audit record.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuditRecord {
    pub event_id: Option<String>,
    pub category: AuditCategory,
    pub decision: String,
    pub detail: String,
}

/// Redacts configured secret/config values before text reaches logs.
#[derive(Clone, Default)]
pub struct SecretRedactor {
    secrets: Vec<String>,
}

impl std::fmt::Debug for SecretRedactor {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SecretRedactor")
            .field("secret_count", &self.secrets.len())
            .finish()
    }
}

impl SecretRedactor {
    pub fn new<I, S>(secrets: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        let secrets = secrets
            .into_iter()
            .map(Into::into)
            .filter(|secret: &String| !secret.is_empty())
            .collect();
        Self { secrets }
    }

    pub fn redact(&self, text: &str) -> String {
        self.secrets.iter().fold(text.to_owned(), |output, secret| {
            output.replace(secret, "[REDACTED]")
        })
    }

    /// Convert an error to log-safe text without exposing configured secrets.
    pub fn redact_error(&self, error: &(dyn std::error::Error + 'static)) -> String {
        self.redact(&error.to_string())
    }

    pub fn record(
        &self,
        event_id: Option<&str>,
        category: AuditCategory,
        decision: impl Into<String>,
        detail: impl AsRef<str>,
    ) -> AuditRecord {
        let decision = decision.into();
        AuditRecord {
            event_id: event_id.map(str::to_owned),
            category,
            decision: self.redact(&decision),
            detail: self.redact(detail.as_ref()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn latency_metric_group_exists() {
        assert_eq!(MetricGroup::Latency, MetricGroup::Latency);
    }

    #[test]
    fn configured_secrets_are_redacted_from_audit_text() {
        let redactor = SecretRedactor::new(["api-secret-123", "session-secret-456"]);
        let record = redactor.record(
            Some("evt-1"),
            AuditCategory::Authorization,
            "rejected api-secret-123",
            "token=api-secret-123 session=session-secret-456",
        );

        assert_eq!(record.decision, "rejected [REDACTED]");
        assert_eq!(record.detail, "token=[REDACTED] session=[REDACTED]");
        assert!(!format!("{record:?}").contains("api-secret-123"));
        assert!(!format!("{record:?}").contains("session-secret-456"));
    }

    #[test]
    fn configured_secrets_are_redacted_from_error_log_text() {
        #[derive(Debug)]
        struct ExampleError;

        impl std::fmt::Display for ExampleError {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str("backend rejected token=api-secret-123")
            }
        }

        impl std::error::Error for ExampleError {}

        let redactor = SecretRedactor::new(["api-secret-123"]);
        assert_eq!(
            redactor.redact_error(&ExampleError),
            "backend rejected token=[REDACTED]"
        );
    }
}
