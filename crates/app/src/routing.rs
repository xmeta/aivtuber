use crate::AppError;
use aivtuber_domain::{
    EventEnvelope, EventKind, MAX_THINKING_TEXT_BYTES, PrivacyClass, ReflexContext, ReflexRequest,
    RetrievalCandidateContext, RetrievalSnapshot, SourceClass, ThinkingRequest,
};
use aivtuber_generative::GenerationRoutingReason;
use aivtuber_reflex::{DecisionReplayRecord, ExecutedAction, ReflexPipeline, ReflexPipelineInput};

/// Maximum bytes accepted for a single template slot value. Untrusted event
/// content larger than this is rejected before interpolation (issue #54).
pub const MAX_TEMPLATE_SLOT_BYTES: usize = 256;
/// Maximum number of slots a single template may bind.
pub const MAX_TEMPLATE_SLOTS: usize = 8;
/// Maximum rendered template output bytes before the output gate.
pub const MAX_TEMPLATE_RENDER_BYTES: usize = 1024;

#[derive(Debug, Clone, PartialEq)]
pub struct GenerationRoute {
    pub source_event: EventEnvelope,
    pub thinking: ThinkingRequest,
    pub routing_reason: GenerationRoutingReason,
    pub intent: String,
    pub style: Option<String>,
    pub fallback_variant_group: Option<String>,
}

/// A curated, versioned response template with named `{slot}` placeholders.
/// Templates are trusted inputs; slot values sourced from events are not.
#[derive(Debug, Clone, PartialEq)]
pub struct ResponseTemplate {
    /// Stable template identifier recorded in replay/telemetry.
    pub id: String,
    /// Template pack version recorded in replay/telemetry.
    pub version: String,
    /// Rendered text with `{slot}` placeholders.
    pub text: String,
    /// Slot names the template binds, e.g. `"name"`, `"count"`.
    pub slots: Vec<String>,
}

/// Deterministic fallback recorded when a template is missing, invalid, or
/// cannot be rendered: the route degrades to `PlaybackRoute::Silent` with the
/// same outcome telemetry as an explicit silent reflex decision.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TemplateFallback {
    MissingTemplate,
    InvalidTemplate,
    SlotLimitExceeded,
    SlotValueTooLarge,
    RenderTooLarge,
}

impl TemplateFallback {
    pub fn reason(self) -> &'static str {
        match self {
            Self::MissingTemplate => "template_missing",
            Self::InvalidTemplate => "template_invalid",
            Self::SlotLimitExceeded => "template_slot_limit",
            Self::SlotValueTooLarge => "template_slot_too_large",
            Self::RenderTooLarge => "template_render_too_large",
        }
    }
}

/// Outcome of composing a reflex Template decision into a response plan.
#[derive(Debug, Clone, PartialEq)]
pub struct TemplateComposition {
    pub template_id: String,
    pub template_version: String,
    /// Fully rendered text with all slots bound.
    pub text: String,
}

/// Curated template pack. Lookup is deterministic: exact asset-id style keys
/// resolved in insertion order with the id as the stable tie-break.
#[derive(Debug, Clone, Default)]
pub struct TemplatePack {
    templates: Vec<ResponseTemplate>,
}

impl TemplatePack {
    pub fn new(templates: Vec<ResponseTemplate>) -> Self {
        Self { templates }
    }

    pub fn get(&self, id: &str) -> Option<&ResponseTemplate> {
        self.templates.iter().find(|template| template.id == id)
    }

    pub fn len(&self) -> usize {
        self.templates.len()
    }

    pub fn is_empty(&self) -> bool {
        self.templates.is_empty()
    }
}

/// Deterministic template composer: binds curated event slots into a
/// validated template and applies hard byte bounds (issue #54).
#[derive(Debug, Clone, Default)]
pub struct TemplateComposer;

impl TemplateComposer {
    /// Resolve `template_id` against `pack` and render it with slots sourced
    /// from trusted/curated event payload fields. Missing/invalid templates or
    /// oversized inputs return a deterministic `TemplateFallback`.
    pub fn compose(
        &self,
        pack: &TemplatePack,
        template_id: &str,
        event: &EventEnvelope,
        slots: &[(String, String)],
    ) -> Result<TemplateComposition, TemplateFallback> {
        let template = pack
            .get(template_id)
            .ok_or(TemplateFallback::MissingTemplate)?;
        self.render(template, event, slots)
    }

    /// Render one known template. Slot count and value sizes are bounded;
    /// the rendered output is trimmed and bounded before returning.
    pub fn render(
        &self,
        template: &ResponseTemplate,
        _event: &EventEnvelope,
        slots: &[(String, String)],
    ) -> Result<TemplateComposition, TemplateFallback> {
        if template.id.trim().is_empty()
            || template.version.trim().is_empty()
            || template.text.trim().is_empty()
        {
            return Err(TemplateFallback::InvalidTemplate);
        }
        if slots.len() > MAX_TEMPLATE_SLOTS {
            return Err(TemplateFallback::SlotLimitExceeded);
        }
        for (name, value) in slots {
            if name.len() > 64 || value.len() > MAX_TEMPLATE_SLOT_BYTES {
                return Err(TemplateFallback::SlotValueTooLarge);
            }
        }

        // Only declared slot names are bindable; unknown slot keys are
        // ignored so untrusted event content cannot inject arbitrary text.
        let declared: Vec<&str> = template.slots.iter().map(String::as_str).collect();
        let mut rendered = template.text.clone();
        for (name, value) in slots {
            if !declared.contains(&name.as_str()) {
                continue;
            }
            // Slot values are normalized: trimmed, control characters
            // stripped, and bounded (already checked above).
            let normalized: String = value
                .trim()
                .chars()
                .filter(|character| !character.is_control())
                .collect();
            rendered = rendered.replace(&format!("{{{name}}}"), &normalized);
        }

        if rendered.len() > MAX_TEMPLATE_RENDER_BYTES {
            return Err(TemplateFallback::RenderTooLarge);
        }
        // Any unresolved `{slot}` placeholder is invalid output.
        if rendered.contains('{') && rendered.contains('}') {
            // Permit other braces? No: templates must not leak placeholders.
            // Strip remaining placeholders deterministically by removing
            // `{...}` groups, matching the invalid-template contract.
            if remove_placeholders(&rendered) != rendered {
                return Err(TemplateFallback::InvalidTemplate);
            }
        }
        let rendered = rendered.trim().to_owned();
        if rendered.is_empty() {
            return Err(TemplateFallback::InvalidTemplate);
        }

        Ok(TemplateComposition {
            template_id: template.id.clone(),
            template_version: template.version.clone(),
            text: rendered,
        })
    }
}

fn remove_placeholders(text: &str) -> String {
    let mut output = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(start) = rest.find('{') {
        output.push_str(&rest[..start]);
        let after = &rest[start..];
        match after.find('}') {
            Some(end) => rest = &after[end + 1..],
            None => {
                output.push('{');
                rest = &after[1..];
            }
        }
    }
    output.push_str(rest);
    output
}

#[derive(Debug, Clone, PartialEq)]
pub enum PlaybackRoute {
    Silent,
    Intent(String),
    AssetId(String),
    AssetIdentity {
        asset_id: String,
        asset_identity: String,
    },
    Template {
        template_id: String,
        composition: TemplateComposition,
    },
    Generate(Box<GenerationRoute>),
}

pub trait RoutePlanner: Send {
    fn route(&mut self, event: &EventEnvelope) -> Result<PlaybackRoute, AppError>;

    fn decision_record(&self) -> Option<&DecisionReplayRecord> {
        None
    }

    /// Deterministic fallback recorded by the most recent Template decision
    /// (`None` when the last decision was not a Template fallback).
    fn template_fallback(&self) -> Option<TemplateFallback> {
        None
    }
}

#[derive(Debug, Default, Clone, Copy)]
pub struct IntentRoutePlanner;

impl RoutePlanner for IntentRoutePlanner {
    fn route(&mut self, event: &EventEnvelope) -> Result<PlaybackRoute, AppError> {
        Ok(event
            .payload
            .get("intent")
            .and_then(serde_json::Value::as_str)
            .filter(|intent| !intent.trim().is_empty())
            .map(|intent| PlaybackRoute::Intent(intent.to_owned()))
            .unwrap_or(PlaybackRoute::Silent))
    }
}

pub trait QueryEmbeddingProvider: Send {
    fn embedding(&mut self, event: &EventEnvelope) -> Result<Vec<f32>, AppError>;
}

pub struct ReflexRoutePlanner<E>
where
    E: QueryEmbeddingProvider,
{
    pipeline: ReflexPipeline,
    embeddings: E,
    context: ReflexContext,
    last_record: Option<DecisionReplayRecord>,
    /// Curated template pack backing ExecutedAction::Template (issue #54).
    templates: TemplatePack,
    composer: TemplateComposer,
    /// Deterministic fallback recorded for the last Template decision, for
    /// telemetry/replay; `None` when the last route was not a fallback.
    last_template_fallback: Option<TemplateFallback>,
}

impl<E> ReflexRoutePlanner<E>
where
    E: QueryEmbeddingProvider,
{
    pub fn new(pipeline: ReflexPipeline, embeddings: E) -> Self {
        Self {
            pipeline,
            embeddings,
            context: ReflexContext::default(),
            last_record: None,
            templates: TemplatePack::default(),
            composer: TemplateComposer,
            last_template_fallback: None,
        }
    }

    /// Install the curated template pack backing Template decisions.
    pub fn with_template_pack(mut self, templates: TemplatePack) -> Self {
        self.templates = templates;
        self
    }

    pub fn set_context(&mut self, context: ReflexContext) {
        self.context = context;
    }

    pub fn last_record(&self) -> Option<&DecisionReplayRecord> {
        self.last_record.as_ref()
    }

    /// Deterministic fallback recorded for the most recent Template decision
    /// (`None` if the last decision was not a fallback).
    pub fn last_template_fallback(&self) -> Option<TemplateFallback> {
        self.last_template_fallback
    }

    /// Compose a Template decision against the curated pack. Slots are sourced
    /// deterministically from trusted event payload fields named after the
    /// template's declared slots, bounded by the composer. On any fallback
    /// the reason is recorded and the route degrades to Silence.
    fn template_route(&mut self, event: &EventEnvelope, template_id: &str) -> PlaybackRoute {
        let slots: Vec<(String, String)> = self
            .templates
            .get(template_id)
            .map(|template| {
                template
                    .slots
                    .iter()
                    .filter_map(|slot| {
                        event
                            .payload
                            .get(slot.as_str())
                            .and_then(serde_json::Value::as_str)
                            .map(|value| (slot.clone(), value.to_owned()))
                    })
                    .collect()
            })
            .unwrap_or_default();

        match self
            .composer
            .compose(&self.templates, template_id, event, &slots)
        {
            Ok(composition) => {
                self.last_template_fallback = None;
                PlaybackRoute::Template {
                    template_id: composition.template_id.clone(),
                    composition,
                }
            }
            Err(fallback) => {
                self.last_template_fallback = Some(fallback);
                PlaybackRoute::Silent
            }
        }
    }
}

impl<E> RoutePlanner for ReflexRoutePlanner<E>
where
    E: QueryEmbeddingProvider,
{
    fn template_fallback(&self) -> Option<TemplateFallback> {
        self.last_template_fallback
    }

    fn route(&mut self, event: &EventEnvelope) -> Result<PlaybackRoute, AppError> {
        let query_embedding = self.embeddings.embedding(event)?;
        let record = self
            .pipeline
            .run(ReflexPipelineInput {
                request: ReflexRequest::new(event.clone(), self.context.clone()),
                query_embedding,
            })
            .map_err(|error| AppError::Routing(error.to_string()))?;

        let route = match record.executed.action {
            ExecutedAction::Cached => match (
                record.executed.asset_id.clone(),
                record.executed.asset_identity.clone(),
            ) {
                (Some(asset_id), Some(asset_identity)) => PlaybackRoute::AssetIdentity {
                    asset_id,
                    asset_identity,
                },
                (Some(asset_id), None) => PlaybackRoute::AssetId(asset_id),
                (None, _) => PlaybackRoute::Silent,
            },
            ExecutedAction::Reaction => event
                .payload
                .get("intent")
                .and_then(serde_json::Value::as_str)
                .filter(|intent| !intent.trim().is_empty())
                .map(|intent| PlaybackRoute::Intent(intent.to_owned()))
                .unwrap_or(PlaybackRoute::Silent),
            ExecutedAction::Llm => {
                PlaybackRoute::Generate(Box::new(generation_route(event, &self.context, &record)?))
            }
            ExecutedAction::Template => match event
                .payload
                .get("template_id")
                .and_then(serde_json::Value::as_str)
                .filter(|value| !value.trim().is_empty())
            {
                Some(template_id) => self.template_route(event, template_id),
                None => {
                    self.last_template_fallback = Some(TemplateFallback::MissingTemplate);
                    PlaybackRoute::Silent
                }
            },
            ExecutedAction::Silent | ExecutedAction::Fallback => {
                self.last_template_fallback = None;
                PlaybackRoute::Silent
            }
        };

        if !matches!(record.executed.action, ExecutedAction::Template) {
            self.last_template_fallback = None;
        }
        self.last_record = Some(record);
        Ok(route)
    }

    fn decision_record(&self) -> Option<&DecisionReplayRecord> {
        self.last_record.as_ref()
    }
}

fn generation_route(
    event: &EventEnvelope,
    context: &ReflexContext,
    record: &DecisionReplayRecord,
) -> Result<GenerationRoute, AppError> {
    let text = bounded_current_event_text(event)?;
    let retrieval = RetrievalSnapshot {
        candidates: record
            .evidence
            .retrieval
            .candidates
            .iter()
            .map(|candidate| RetrievalCandidateContext {
                asset_id: candidate.asset_id.clone(),
                rank: candidate.rank,
                similarity: candidate.similarity,
            })
            .collect(),
    };
    let thinking = ThinkingRequest::from_event(
        event,
        text,
        privacy_for_source(event.source_class),
        context.clone(),
        retrieval,
        Vec::new(),
    );
    thinking.validate().map_err(|error| {
        AppError::Routing(format!("invalid production thinking request: {error}"))
    })?;

    let intent = event
        .payload
        .get("intent")
        .and_then(serde_json::Value::as_str)
        .filter(|value| !value.trim().is_empty())
        .unwrap_or("generated.reply")
        .to_owned();
    let fallback_variant_group = record
        .evidence
        .normalized
        .reaction_family
        .as_deref()
        .filter(|family| !family.trim().is_empty())
        .map(|family| {
            if family.contains('.') {
                family.to_owned()
            } else {
                format!("reaction.{family}")
            }
        });
    Ok(GenerationRoute {
        source_event: event.clone(),
        thinking,
        routing_reason: GenerationRoutingReason::ExplicitLlmRoute,
        intent,
        style: None,
        fallback_variant_group,
    })
}

fn bounded_current_event_text(event: &EventEnvelope) -> Result<String, AppError> {
    let candidate = ["text", "message", "title", "intent"]
        .into_iter()
        .find_map(|key| {
            event
                .payload
                .get(key)
                .and_then(serde_json::Value::as_str)
                .map(str::trim)
                .filter(|value| !value.is_empty())
        })
        .unwrap_or_else(|| event_kind_label(event.kind));
    let bounded = truncate_utf8(candidate, MAX_THINKING_TEXT_BYTES);
    if bounded.is_empty() {
        return Err(AppError::Routing(
            "generation requires non-empty curated current-event text".to_owned(),
        ));
    }
    Ok(bounded.to_owned())
}

fn truncate_utf8(value: &str, max_bytes: usize) -> &str {
    if value.len() <= max_bytes {
        return value;
    }
    let mut end = max_bytes;
    while !value.is_char_boundary(end) {
        end -= 1;
    }
    &value[..end]
}

fn privacy_for_source(source: SourceClass) -> PrivacyClass {
    match source {
        SourceClass::PublicChat | SourceClass::Donation | SourceClass::Speech => {
            PrivacyClass::Pseudonymous
        }
        SourceClass::Operator => PrivacyClass::Private,
        SourceClass::Game | SourceClass::Stream | SourceClass::Timer | SourceClass::System => {
            PrivacyClass::Public
        }
    }
}

fn event_kind_label(kind: EventKind) -> &'static str {
    match kind {
        EventKind::ChatMessage => "chat message",
        EventKind::ChatDonation => "chat donation",
        EventKind::SpeechInput => "speech input",
        EventKind::GameEvent => "game event",
        EventKind::StreamEvent => "stream event",
        EventKind::TimerTick => "timer tick",
        EventKind::OperatorCommand => "operator command",
        EventKind::SystemHealth => "system health",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    fn template_pack() -> TemplatePack {
        TemplatePack::new(vec![
            ResponseTemplate {
                id: "thanks.donation".to_owned(),
                version: "curated-v1".to_owned(),
                text: "{name}さん、ありがとう！".to_owned(),
                slots: vec!["name".to_owned()],
            },
            ResponseTemplate {
                id: "goal.count".to_owned(),
                version: "curated-v1".to_owned(),
                text: "あと{count}人で目標達成！".to_owned(),
                slots: vec!["count".to_owned()],
            },
        ])
    }

    fn chat_event(payload: BTreeMap<String, serde_json::Value>) -> EventEnvelope {
        EventEnvelope {
            schema_version: "0.1.0".to_owned(),
            event_id: "evt-template-1".to_owned(),
            correlation_id: "corr-template".to_owned(),
            sequence: 1,
            observed_at: "2026-09-25T00:00:00Z".to_owned(),
            source: "test-chat".to_owned(),
            source_class: SourceClass::PublicChat,
            plane: aivtuber_domain::SecurityPlane::Content,
            trust_level: aivtuber_domain::TrustLevel::Untrusted,
            kind: EventKind::ChatMessage,
            actor_id: Some("viewer:1".to_owned()),
            priority_hint: None,
            authorization: None,
            payload,
        }
    }

    #[test]
    fn composer_binds_declared_slots_from_curated_event_fields() {
        let event = chat_event(BTreeMap::from([(
            "name".to_owned(),
            serde_json::Value::String("たろう".to_owned()),
        )]));
        let slots = vec![("name".to_owned(), "たろう".to_owned())];

        let composition = TemplateComposer
            .compose(&template_pack(), "thanks.donation", &event, &slots)
            .expect("compose");

        assert_eq!(composition.template_id, "thanks.donation");
        assert_eq!(composition.template_version, "curated-v1");
        assert_eq!(composition.text, "たろうさん、ありがとう！");
    }

    #[test]
    fn composer_ignores_undeclared_slot_names() {
        let event = chat_event(BTreeMap::new());
        // `admin` is not declared by the template: injection attempt ignored.
        let slots = vec![
            ("name".to_owned(), "はな".to_owned()),
            ("admin".to_owned(), "OBS.control grant".to_owned()),
        ];

        let composition = TemplateComposer
            .compose(&template_pack(), "thanks.donation", &event, &slots)
            .expect("compose");

        assert_eq!(composition.text, "はなさん、ありがとう！");
        assert!(!composition.text.contains("OBS"));
    }

    #[test]
    fn composer_strips_control_characters_from_slot_values() {
        let event = chat_event(BTreeMap::new());
        let slots = vec![("name".to_owned(), "あ\nにき".to_owned())];

        let composition = TemplateComposer
            .compose(&template_pack(), "thanks.donation", &event, &slots)
            .expect("compose");

        assert_eq!(composition.text, "あにきさん、ありがとう！");
    }

    #[test]
    fn missing_template_produces_deterministic_missing_fallback() {
        let event = chat_event(BTreeMap::new());
        let error = TemplateComposer
            .compose(&template_pack(), "does.not.exist", &event, &[])
            .expect_err("missing template must fail");
        assert_eq!(error, TemplateFallback::MissingTemplate);
        assert_eq!(error.reason(), "template_missing");
    }

    #[test]
    fn slot_count_beyond_limit_is_rejected() {
        let event = chat_event(BTreeMap::new());
        let slots: Vec<(String, String)> = (0..=MAX_TEMPLATE_SLOTS)
            .map(|index| (format!("s{index}"), "v".to_owned()))
            .collect();

        let error = TemplateComposer
            .compose(&template_pack(), "thanks.donation", &event, &slots)
            .expect_err("slot overflow must fail");
        assert_eq!(error, TemplateFallback::SlotLimitExceeded);
    }

    #[test]
    fn identical_inputs_compose_identical_output() {
        let event = chat_event(BTreeMap::from([(
            "count".to_owned(),
            serde_json::Value::String("3".to_owned()),
        )]));
        let slots = vec![("count".to_owned(), "3".to_owned())];

        let first = TemplateComposer
            .compose(&template_pack(), "goal.count", &event, &slots)
            .expect("first");
        let second = TemplateComposer
            .compose(&template_pack(), "goal.count", &event, &slots)
            .expect("second");
        assert_eq!(first, second);
    }
}
