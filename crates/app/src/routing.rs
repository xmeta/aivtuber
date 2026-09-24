use crate::AppError;
use aivtuber_domain::{EventEnvelope, ReflexContext, ReflexRequest};
use aivtuber_reflex::{DecisionReplayRecord, ExecutedAction, ReflexPipeline, ReflexPipelineInput};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PlaybackRoute {
    Silent,
    Intent(String),
    AssetId(String),
}

pub trait RoutePlanner: Send {
    fn route(&mut self, event: &EventEnvelope) -> Result<PlaybackRoute, AppError>;
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
            ExecutedAction::Cached => record
                .executed
                .asset_id
                .clone()
                .map(PlaybackRoute::AssetId)
                .unwrap_or(PlaybackRoute::Silent),
            ExecutedAction::Reaction => event
                .payload
                .get("intent")
                .and_then(serde_json::Value::as_str)
                .filter(|intent| !intent.trim().is_empty())
                .map(|intent| PlaybackRoute::Intent(intent.to_owned()))
                .unwrap_or(PlaybackRoute::Silent),
            ExecutedAction::Silent
            | ExecutedAction::Template
            | ExecutedAction::Llm
            | ExecutedAction::Fallback => PlaybackRoute::Silent,
        };

        self.last_record = Some(record);
        Ok(route)
    }
}
