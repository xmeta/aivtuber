use crate::AppError;
use aivtuber_domain::{
    EventEnvelope, EventKind, MAX_THINKING_TEXT_BYTES, PrivacyClass, ReflexContext, ReflexRequest,
    RetrievalCandidateContext, RetrievalSnapshot, SourceClass, ThinkingRequest,
};
use aivtuber_generative::GenerationRoutingReason;
use aivtuber_reflex::{DecisionReplayRecord, ExecutedAction, ReflexPipeline, ReflexPipelineInput};

#[derive(Debug, Clone, PartialEq)]
pub struct GenerationRoute {
    pub source_event: EventEnvelope,
    pub thinking: ThinkingRequest,
    pub routing_reason: GenerationRoutingReason,
    pub intent: String,
    pub style: Option<String>,
    pub fallback_variant_group: Option<String>,
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
    Generate(Box<GenerationRoute>),
}

pub trait RoutePlanner: Send {
    fn route(&mut self, event: &EventEnvelope) -> Result<PlaybackRoute, AppError>;

    fn decision_record(&self) -> Option<&DecisionReplayRecord> {
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
        }
    }

    pub fn set_context(&mut self, context: ReflexContext) {
        self.context = context;
    }

    pub fn last_record(&self) -> Option<&DecisionReplayRecord> {
        self.last_record.as_ref()
    }
}

impl<E> RoutePlanner for ReflexRoutePlanner<E>
where
    E: QueryEmbeddingProvider,
{
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
            ExecutedAction::Silent | ExecutedAction::Template | ExecutedAction::Fallback => {
                PlaybackRoute::Silent
            }
        };

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
