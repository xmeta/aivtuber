//! Live switching of the local Thinking model (issue #357).
//!
//! `aivtuber-setup start` serves every installed model through llama-server's
//! router mode and tells the app which aliases exist (`ThinkingModelCatalog`,
//! the allowlist) and which one to begin on. This module owns the one piece of
//! runtime state that follows: which model the next request names.
//!
//! * The only entry point is [`ModelSwitcher::select`], reached through
//!   `ProductionApp::select_thinking_model`, which demands the
//!   `model.select` capability minted by `LocalControlIngress` (the same path
//!   as stop and mute, audited the same way). A model request that is not in
//!   the allowlist changes nothing.
//! * A switch points the adapter at the new model at once and runs the
//!   existing Thinking warm-up on a background thread, so the weights load off
//!   the reply path. The composition loop only ever polls a channel: it never
//!   waits for a load. Replies that arrive while the model loads use the
//!   normal Thinking timeout and degrade to the typed fallback, exactly as a
//!   cold start does.
//! * If the warm-up fails (the file is broken, the server is gone), the
//!   adapter is pointed back at the previous model and the failure is
//!   reported; the choice is not remembered.
//! * The remembered choice is one bounded file holding an alias (see
//!   `aivtuber_domain::thinking_model`), written only after a successful
//!   warm-up.
//!
//! Model output is data here as everywhere: nothing a model says can select a
//! model, and the alias that reaches the server is always one the launcher
//! listed.

use aivtuber_adapters::OpenAiResponsesAdapter;
use aivtuber_domain::{
    ThinkingModel, ThinkingModelCatalog, ThinkingModelError, write_model_settings,
};
use serde::{Deserialize, Serialize};
use std::fmt;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::mpsc::{Receiver, TryRecvError, channel};
use std::time::Duration;

/// How the warm-up that follows a switch is run: `true` when the model
/// answered. Injected so tests need no model server; production runs the
/// app's own [`crate::thinking_warmup`] against a clone of the adapter.
pub type WarmFn = Arc<dyn Fn(Duration) -> bool + Send + Sync>;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ModelSwitchError {
    /// Not a model alias, or not in the allowlist.
    Refused(ThinkingModelError),
    /// One switch at a time: the previous load has not finished.
    Busy,
    /// The warm-up thread could not be started.
    Unavailable,
}

impl fmt::Display for ModelSwitchError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Refused(error) => write!(f, "refused: {error}"),
            Self::Busy => f.write_str("refused: a model switch is already in progress"),
            Self::Unavailable => f.write_str("refused: cannot start the model warm-up"),
        }
    }
}

enum State {
    Ready,
    Switching {
        target: ThinkingModel,
        previous: ThinkingModel,
        done: Receiver<bool>,
    },
    /// The last switch failed; the previous model is active again.
    Failed {
        target: String,
    },
}

/// What the operator sees (panel and `aivtuberctl status`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ThinkingModelStatus {
    pub active: String,
    pub active_version: String,
    /// `ready`, `switching` or `failed`.
    pub state: String,
    /// The model being loaded, or the one that failed to load.
    pub target: Option<String>,
    pub available: Vec<ThinkingModel>,
}

pub struct ModelSwitcher {
    catalog: ThinkingModelCatalog,
    adapter: OpenAiResponsesAdapter,
    settings: Option<PathBuf>,
    warmup_timeout: Duration,
    warm: WarmFn,
    active: ThinkingModel,
    state: State,
}

impl fmt::Debug for ModelSwitcher {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ModelSwitcher")
            .field("active", &self.active.alias)
            .field("catalog", &self.catalog.render_choices())
            .finish_non_exhaustive()
    }
}

impl ModelSwitcher {
    /// `adapter` is a clone of the one the pipeline owns (clones share the
    /// model slot). Its current model must be in `catalog`; its version is
    /// aligned with the catalog's.
    pub fn new(
        catalog: ThinkingModelCatalog,
        adapter: OpenAiResponsesAdapter,
        settings: Option<PathBuf>,
        warmup_timeout: Duration,
    ) -> Result<Self, String> {
        let warm_adapter = adapter.clone();
        let warm: WarmFn = Arc::new(move |timeout| {
            crate::thinking_warmup::run(&warm_adapter, timeout, &|line| eprintln!("{line}"))
        });
        Self::with_warmup(catalog, adapter, settings, warmup_timeout, warm)
    }

    pub fn with_warmup(
        catalog: ThinkingModelCatalog,
        adapter: OpenAiResponsesAdapter,
        settings: Option<PathBuf>,
        warmup_timeout: Duration,
        warm: WarmFn,
    ) -> Result<Self, String> {
        let current = adapter.active_model().alias;
        let active = catalog.get(&current).cloned().ok_or_else(|| {
            format!("AIVTUBER_OPENAI_MODEL {current:?} is not in the model choices")
        })?;
        adapter
            .select_model(&active.alias, Some(&active.version))
            .map_err(|e| e.to_string())?;
        Ok(Self {
            catalog,
            adapter,
            settings,
            warmup_timeout,
            warm,
            active,
            state: State::Ready,
        })
    }

    pub fn catalog(&self) -> &ThinkingModelCatalog {
        &self.catalog
    }

    /// Switch to `alias`. Returns at once; the load runs on a background
    /// thread and is reported through [`Self::status`].
    pub fn select(&mut self, alias: &str) -> Result<String, ModelSwitchError> {
        self.poll();
        let target = self
            .catalog
            .select(alias)
            .map_err(ModelSwitchError::Refused)?
            .clone();
        if matches!(self.state, State::Switching { .. }) {
            return Err(ModelSwitchError::Busy);
        }
        if target == self.active && !matches!(self.state, State::Failed { .. }) {
            return Ok(format!("already using {}", target.alias));
        }
        let previous = self.active.clone();
        let (sender, done) = channel();
        let warm = Arc::clone(&self.warm);
        let timeout = self.warmup_timeout;
        // From here a request names the new model; the server loads it on
        // demand and the warm-up below is what waits for that.
        self.adapter
            .select_model(&target.alias, Some(&target.version))
            .map_err(|_| ModelSwitchError::Unavailable)?;
        let spawned = std::thread::Builder::new()
            .name("thinking-model-switch".to_owned())
            .spawn(move || {
                let _ = sender.send(warm(timeout));
            });
        if spawned.is_err() {
            let _ = self
                .adapter
                .select_model(&previous.alias, Some(&previous.version));
            return Err(ModelSwitchError::Unavailable);
        }
        let detail = format!("switching to {} (loading)", target.alias);
        self.state = State::Switching {
            target,
            previous,
            done,
        };
        Ok(detail)
    }

    /// Collect the result of a finished warm-up. Never blocks.
    pub fn poll(&mut self) {
        let State::Switching { done, .. } = &self.state else {
            return;
        };
        let outcome = match done.try_recv() {
            Ok(ok) => ok,
            Err(TryRecvError::Empty) => return,
            // The thread died without an answer: treat it as a failed load.
            Err(TryRecvError::Disconnected) => false,
        };
        let State::Switching {
            target, previous, ..
        } = std::mem::replace(&mut self.state, State::Ready)
        else {
            return;
        };
        if outcome {
            if let Some(path) = &self.settings
                && let Err(error) = write_model_settings(path, &target.alias)
            {
                eprintln!("model switch: could not remember the choice: {error}");
            }
            eprintln!("model switch: ready model={}", target.alias);
            self.active = target;
        } else {
            let _ = self
                .adapter
                .select_model(&previous.alias, Some(&previous.version));
            eprintln!(
                "model switch: failed model={} (back on {})",
                target.alias, previous.alias
            );
            self.state = State::Failed {
                target: target.alias,
            };
        }
    }

    pub fn status(&self) -> ThinkingModelStatus {
        let (state, target, shown) = match &self.state {
            State::Ready => ("ready", None, &self.active),
            State::Switching { target, .. } => ("switching", Some(target.alias.clone()), target),
            State::Failed { target } => ("failed", Some(target.clone()), &self.active),
        };
        ThinkingModelStatus {
            active: shown.alias.clone(),
            active_version: shown.version.clone(),
            state: state.to_owned(),
            target,
            available: self.catalog.models().to_vec(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use aivtuber_adapters::{OpenAiResponsesConfig, ThinkingAuthentication};
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::time::Instant;

    fn catalog() -> ThinkingModelCatalog {
        ThinkingModelCatalog::parse_choices("alpha:v1,beta:v2").unwrap()
    }

    fn adapter(alias: &str) -> OpenAiResponsesAdapter {
        OpenAiResponsesAdapter::with_authentication(
            OpenAiResponsesConfig {
                endpoint: "http://127.0.0.1:9/v1/responses".to_owned(),
                model_alias: alias.to_owned(),
                model_version: Some("stale".to_owned()),
                ..OpenAiResponsesConfig::default()
            },
            ThinkingAuthentication::UnauthenticatedLoopback,
        )
        .unwrap()
    }

    fn switcher(
        settings: Option<PathBuf>,
        ok: Arc<AtomicBool>,
        gate: Option<Arc<AtomicBool>>,
    ) -> (ModelSwitcher, OpenAiResponsesAdapter) {
        let shared = adapter("alpha");
        let warm: WarmFn = Arc::new(move |_| {
            // A held-open "load": the switch stays in progress until released.
            while gate.as_ref().is_some_and(|g| !g.load(Ordering::SeqCst)) {
                std::thread::sleep(Duration::from_millis(2));
            }
            ok.load(Ordering::SeqCst)
        });
        let s = ModelSwitcher::with_warmup(
            catalog(),
            shared.clone(),
            settings,
            Duration::from_secs(1),
            warm,
        )
        .unwrap();
        (s, shared)
    }

    fn wait_for(s: &mut ModelSwitcher, state: &str) {
        let deadline = Instant::now() + Duration::from_secs(5);
        while s.status().state != state {
            assert!(Instant::now() < deadline, "stuck in {:?}", s.status());
            s.poll();
            std::thread::sleep(Duration::from_millis(2));
        }
    }

    #[test]
    fn it_starts_on_the_configured_model_with_the_catalog_version() {
        let (s, shared) = switcher(None, Arc::new(AtomicBool::new(true)), None);
        let status = s.status();
        assert_eq!(
            (status.active.as_str(), status.state.as_str()),
            ("alpha", "ready")
        );
        assert_eq!(status.active_version, "v1");
        assert_eq!(status.available.len(), 2);
        assert_eq!(shared.active_model().version.as_deref(), Some("v1"));
        // A configured model outside the allowlist is a startup error.
        let err = ModelSwitcher::with_warmup(
            catalog(),
            adapter("gamma"),
            None,
            Duration::from_secs(1),
            Arc::new(|_| true),
        )
        .unwrap_err();
        assert!(err.contains("gamma"), "{err}");
    }

    #[test]
    fn a_name_outside_the_allowlist_changes_nothing() {
        let (mut s, shared) = switcher(None, Arc::new(AtomicBool::new(true)), None);
        for bad in ["gamma", "", "../alpha", "ALPHA", "alpha ", "C:\\x.gguf"] {
            assert!(
                matches!(s.select(bad), Err(ModelSwitchError::Refused(_))),
                "{bad:?}"
            );
        }
        assert_eq!(shared.active_model().alias, "alpha");
        assert_eq!(s.status().state, "ready");
    }

    #[test]
    fn a_switch_is_immediate_for_requests_and_ready_after_the_warm_up() {
        let dir = std::env::temp_dir().join(format!("aivtuber-switch-ok-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("model.json");
        let gate = Arc::new(AtomicBool::new(false));
        let (mut s, shared) = switcher(
            Some(path.clone()),
            Arc::new(AtomicBool::new(true)),
            Some(Arc::clone(&gate)),
        );
        assert_eq!(s.select("alpha").unwrap(), "already using alpha");
        assert!(s.select("beta").unwrap().starts_with("switching to beta"));
        // While the model loads: requests already name it, the status says so,
        // another switch is refused, and nothing is remembered yet.
        assert_eq!(shared.active_model().alias, "beta");
        let status = s.status();
        assert_eq!(
            (
                status.state.as_str(),
                status.target.as_deref(),
                status.active.as_str()
            ),
            ("switching", Some("beta"), "beta")
        );
        assert_eq!(s.select("alpha"), Err(ModelSwitchError::Busy));
        assert!(!path.exists());
        gate.store(true, Ordering::SeqCst);
        wait_for(&mut s, "ready");
        let status = s.status();
        assert_eq!(
            (status.active.as_str(), status.active_version.as_str()),
            ("beta", "v2")
        );
        assert_eq!(shared.active_model().version.as_deref(), Some("v2"));
        // The choice is remembered, and only as an allowlisted alias.
        let saved = std::fs::read_to_string(&path).unwrap();
        assert_eq!(
            aivtuber_domain::parse_model_settings(saved.as_bytes()).as_deref(),
            Some("beta")
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_failed_load_goes_back_to_the_previous_model_and_is_not_remembered() {
        let dir = std::env::temp_dir().join(format!("aivtuber-switch-bad-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("model.json");
        let (mut s, shared) = switcher(Some(path.clone()), Arc::new(AtomicBool::new(false)), None);
        s.select("beta").unwrap();
        wait_for(&mut s, "failed");
        let status = s.status();
        assert_eq!(
            (status.active.as_str(), status.target.as_deref()),
            ("alpha", Some("beta"))
        );
        assert_eq!(shared.active_model().alias, "alpha");
        assert_eq!(shared.active_model().version.as_deref(), Some("v1"));
        assert!(!path.exists());
        // After a failure the operator may try again (here, the same model).
        assert!(s.select("beta").unwrap().starts_with("switching to beta"));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
