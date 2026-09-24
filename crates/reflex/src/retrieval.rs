use serde::{Deserialize, Serialize};
use std::cmp::Ordering;
use std::error::Error;
use std::fmt;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SimilarityMetric {
    Cosine,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RetrievalMetadata {
    pub retriever_version: String,
    pub embedding_model: String,
    pub index_version: String,
    pub similarity_metric: SimilarityMetric,
    pub tie_break_rule: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RetrievalCandidate {
    pub asset_id: String,
    pub rank: u32,
    pub similarity: f64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RetrievalResult {
    pub metadata: RetrievalMetadata,
    pub candidates: Vec<RetrievalCandidate>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct IndexedAsset {
    pub asset_id: String,
    pub embedding: Vec<f32>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RetrievalError {
    EmptyIndex,
    EmptyAssetId,
    ZeroDimension,
    DimensionMismatch { expected: usize, actual: usize },
    NonFiniteVector,
    ZeroNormVector,
}

impl fmt::Display for RetrievalError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::EmptyIndex => f.write_str("semantic index must contain at least one asset"),
            Self::EmptyAssetId => f.write_str("semantic index asset id must not be empty"),
            Self::ZeroDimension => f.write_str("semantic embedding dimension must be positive"),
            Self::DimensionMismatch { expected, actual } => {
                write!(
                    f,
                    "embedding dimension mismatch: expected {expected}, got {actual}"
                )
            }
            Self::NonFiniteVector => f.write_str("embedding vector contains a non-finite value"),
            Self::ZeroNormVector => f.write_str("embedding vector must have non-zero norm"),
        }
    }
}

impl Error for RetrievalError {}

#[derive(Debug, Clone)]
pub struct SemanticIndex {
    metadata: RetrievalMetadata,
    dimension: usize,
    assets: Vec<IndexedAsset>,
}

impl SemanticIndex {
    pub fn new(
        metadata: RetrievalMetadata,
        mut assets: Vec<IndexedAsset>,
    ) -> Result<Self, RetrievalError> {
        if assets.is_empty() {
            return Err(RetrievalError::EmptyIndex);
        }
        let dimension = assets[0].embedding.len();
        if dimension == 0 {
            return Err(RetrievalError::ZeroDimension);
        }

        for asset in &assets {
            if asset.asset_id.trim().is_empty() {
                return Err(RetrievalError::EmptyAssetId);
            }
            validate_vector(&asset.embedding, dimension)?;
        }

        assets.sort_by(|left, right| left.asset_id.cmp(&right.asset_id));
        assets.dedup_by(|left, right| left.asset_id == right.asset_id);

        Ok(Self {
            metadata,
            dimension,
            assets,
        })
    }

    pub fn metadata(&self) -> &RetrievalMetadata {
        &self.metadata
    }

    pub fn search(&self, query: &[f32], top_k: usize) -> Result<RetrievalResult, RetrievalError> {
        validate_vector(query, self.dimension)?;
        if top_k == 0 {
            return Ok(RetrievalResult {
                metadata: self.metadata.clone(),
                candidates: Vec::new(),
            });
        }

        let query_norm = norm(query);
        let mut scored = self
            .assets
            .iter()
            .map(|asset| {
                let similarity =
                    dot(query, &asset.embedding) / (query_norm * norm(&asset.embedding));
                (asset.asset_id.clone(), similarity)
            })
            .collect::<Vec<_>>();

        scored.sort_by(|(left_id, left_score), (right_id, right_score)| {
            right_score
                .partial_cmp(left_score)
                .unwrap_or(Ordering::Equal)
                .then_with(|| left_id.cmp(right_id))
        });

        let candidates = scored
            .into_iter()
            .take(top_k)
            .enumerate()
            .map(|(index, (asset_id, similarity))| RetrievalCandidate {
                asset_id,
                rank: (index + 1) as u32,
                similarity,
            })
            .collect();

        Ok(RetrievalResult {
            metadata: self.metadata.clone(),
            candidates,
        })
    }
}

fn validate_vector(vector: &[f32], expected: usize) -> Result<(), RetrievalError> {
    if vector.len() != expected {
        return Err(RetrievalError::DimensionMismatch {
            expected,
            actual: vector.len(),
        });
    }
    if vector.iter().any(|value| !value.is_finite()) {
        return Err(RetrievalError::NonFiniteVector);
    }
    if norm(vector) == 0.0 {
        return Err(RetrievalError::ZeroNormVector);
    }
    Ok(())
}

fn dot(left: &[f32], right: &[f32]) -> f64 {
    left.iter()
        .zip(right)
        .map(|(left, right)| f64::from(*left) * f64::from(*right))
        .sum()
}

fn norm(vector: &[f32]) -> f64 {
    dot(vector, vector).sqrt()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn metadata() -> RetrievalMetadata {
        RetrievalMetadata {
            retriever_version: "semantic-v1".to_owned(),
            embedding_model: "example-embed-v1".to_owned(),
            index_version: "assets-2026-09-24".to_owned(),
            similarity_metric: SimilarityMetric::Cosine,
            tie_break_rule: "similarity_desc_then_asset_id_asc".to_owned(),
        }
    }

    #[test]
    fn top_k_is_deterministic_and_records_versioned_metadata() {
        let index = SemanticIndex::new(
            metadata(),
            vec![
                IndexedAsset {
                    asset_id: "asset.b".to_owned(),
                    embedding: vec![1.0, 0.0],
                },
                IndexedAsset {
                    asset_id: "asset.a".to_owned(),
                    embedding: vec![1.0, 0.0],
                },
                IndexedAsset {
                    asset_id: "asset.c".to_owned(),
                    embedding: vec![0.0, 1.0],
                },
            ],
        )
        .expect("index");

        let result = index.search(&[1.0, 0.0], 2).expect("search");
        assert_eq!(
            result
                .candidates
                .iter()
                .map(|candidate| candidate.asset_id.as_str())
                .collect::<Vec<_>>(),
            vec!["asset.a", "asset.b"]
        );
        assert_eq!(result.candidates[0].rank, 1);
        assert_eq!(result.metadata.index_version, "assets-2026-09-24");
        assert_eq!(
            result.metadata.tie_break_rule,
            "similarity_desc_then_asset_id_asc"
        );
    }

    #[test]
    fn cosine_similarity_orders_semantic_neighbors() {
        let index = SemanticIndex::new(
            metadata(),
            vec![
                IndexedAsset {
                    asset_id: "surprise".to_owned(),
                    embedding: vec![1.0, 0.0],
                },
                IndexedAsset {
                    asset_id: "gratitude".to_owned(),
                    embedding: vec![0.0, 1.0],
                },
            ],
        )
        .expect("index");

        let result = index.search(&[0.9, 0.1], 2).expect("search");
        assert_eq!(result.candidates[0].asset_id, "surprise");
        assert!(result.candidates[0].similarity > result.candidates[1].similarity);
    }

    #[test]
    fn dimension_mismatch_is_rejected() {
        let index = SemanticIndex::new(
            metadata(),
            vec![IndexedAsset {
                asset_id: "asset.a".to_owned(),
                embedding: vec![1.0, 0.0],
            }],
        )
        .expect("index");

        assert_eq!(
            index.search(&[1.0], 1).expect_err("dimension must fail"),
            RetrievalError::DimensionMismatch {
                expected: 2,
                actual: 1,
            }
        );
    }
}
