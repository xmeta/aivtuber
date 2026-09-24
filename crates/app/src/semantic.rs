use aivtuber_asset_store::{AssetStore, CompatibilityStatus};
use aivtuber_reflex::{
    IndexedAsset, RetrievalError, RetrievalMetadata, SemanticIndex, SimilarityMetric,
};
use std::error::Error;
use std::fmt;

pub const SEMANTIC_TIE_BREAK_RULE: &str = "similarity_desc_then_asset_id_asc";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AssetSemanticIndexConfig {
    pub retriever_version: String,
    pub embedding_model: String,
    pub embedding_model_version: String,
}

impl AssetSemanticIndexConfig {
    pub fn embedding_model_key(&self) -> String {
        format!("{}@{}", self.embedding_model, self.embedding_model_version)
    }

    fn validate(&self) -> Result<(), AssetSemanticIndexError> {
        if self.retriever_version.trim().is_empty()
            || self.embedding_model.trim().is_empty()
            || self.embedding_model_version.trim().is_empty()
        {
            return Err(AssetSemanticIndexError::InvalidConfig(
                "retriever and embedding model versions must not be empty".to_owned(),
            ));
        }
        Ok(())
    }
}

#[derive(Debug)]
pub enum AssetSemanticIndexError {
    InvalidConfig(String),
    NoCompatibleEmbeddings { model: String },
    Retrieval(RetrievalError),
}

impl fmt::Display for AssetSemanticIndexError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidConfig(message) => write!(f, "invalid semantic index config: {message}"),
            Self::NoCompatibleEmbeddings { model } => {
                write!(
                    f,
                    "no compatible Performance Assets have embedding model {model:?}"
                )
            }
            Self::Retrieval(source) => write!(f, "semantic index build failed: {source}"),
        }
    }
}

impl Error for AssetSemanticIndexError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Retrieval(source) => Some(source),
            _ => None,
        }
    }
}

impl From<RetrievalError> for AssetSemanticIndexError {
    fn from(value: RetrievalError) -> Self {
        Self::Retrieval(value)
    }
}

pub fn build_semantic_index_from_asset_store(
    store: &AssetStore,
    config: &AssetSemanticIndexConfig,
) -> Result<SemanticIndex, AssetSemanticIndexError> {
    config.validate()?;
    let model_key = config.embedding_model_key();

    let mut assets = Vec::new();
    for (asset_id, entry) in store.indexed_entries() {
        if !matches!(entry.compatibility, CompatibilityStatus::Usable) {
            continue;
        }
        let Some(embedding) = entry.semantic_embedding.as_ref() else {
            continue;
        };
        if embedding.model_key() != model_key {
            continue;
        }

        assets.push(IndexedAsset {
            asset_id: asset_id.to_owned(),
            asset_identity: Some(entry.identity.stable_key()),
            embedding: embedding.vector.clone(),
        });
    }

    if assets.is_empty() {
        return Err(AssetSemanticIndexError::NoCompatibleEmbeddings { model: model_key });
    }

    let index_version = deterministic_index_version(
        &config.retriever_version,
        &model_key,
        &store.runtime().compiler_version,
        &assets,
    );
    let metadata = RetrievalMetadata {
        retriever_version: config.retriever_version.clone(),
        embedding_model: model_key,
        index_version,
        asset_compiler_version: store.runtime().compiler_version.clone(),
        similarity_metric: SimilarityMetric::Cosine,
        tie_break_rule: SEMANTIC_TIE_BREAK_RULE.to_owned(),
    };

    SemanticIndex::new(metadata, assets).map_err(Into::into)
}

fn deterministic_index_version(
    retriever_version: &str,
    embedding_model: &str,
    asset_compiler_version: &str,
    assets: &[IndexedAsset],
) -> String {
    let mut hash = FNV_OFFSET_BASIS;
    hash_component(&mut hash, retriever_version.as_bytes());
    hash_component(&mut hash, embedding_model.as_bytes());
    hash_component(&mut hash, asset_compiler_version.as_bytes());
    hash_component(&mut hash, SEMANTIC_TIE_BREAK_RULE.as_bytes());

    for asset in assets {
        hash_component(&mut hash, asset.asset_id.as_bytes());
        hash_component(
            &mut hash,
            asset
                .asset_identity
                .as_deref()
                .unwrap_or_default()
                .as_bytes(),
        );
        hash_component(&mut hash, &(asset.embedding.len() as u64).to_le_bytes());
        for value in &asset.embedding {
            hash_component(&mut hash, &value.to_bits().to_le_bytes());
        }
    }

    format!("asset-semantic-fnv1a64-{hash:016x}")
}

const FNV_OFFSET_BASIS: u64 = 0xcbf29ce484222325;
const FNV_PRIME: u64 = 0x00000100000001b3;

fn hash_component(hash: &mut u64, bytes: &[u8]) {
    for byte in (bytes.len() as u64).to_le_bytes().iter().chain(bytes) {
        *hash ^= u64::from(*byte);
        *hash = hash.wrapping_mul(FNV_PRIME);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use aivtuber_asset_store::RuntimeCompatibility;
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicU64, Ordering};

    fn pack_descriptors() -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../examples/starter-reaction-pack/descriptors")
    }

    fn runtime() -> RuntimeCompatibility {
        RuntimeCompatibility {
            compiler_version: "0.1.0".to_owned(),
            voice_model: Some("example-voice-v1".to_owned()),
            avatar_profile: Some("example-live2d-v1".to_owned()),
            viseme_mapping: Some("ja-5vowel-v1".to_owned()),
            motion_library: Some("starter-v1".to_owned()),
        }
    }

    fn config() -> AssetSemanticIndexConfig {
        AssetSemanticIndexConfig {
            retriever_version: "asset-semantic-v1".to_owned(),
            embedding_model: "starter-semantic".to_owned(),
            embedding_model_version: "1".to_owned(),
        }
    }

    struct TestDir(PathBuf);

    impl TestDir {
        fn new(name: &str) -> Self {
            static NEXT: AtomicU64 = AtomicU64::new(0);
            let serial = NEXT.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir().join(format!(
                "aivtuber-semantic-{name}-{}-{serial}",
                std::process::id()
            ));
            fs::create_dir_all(&path).expect("create temp dir");
            Self(path)
        }

        fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for TestDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn copy_descriptor(name: &str, target: &Path) {
        fs::copy(pack_descriptors().join(name), target.join(name)).expect("copy descriptor");
    }

    #[test]
    fn starter_pack_build_is_deterministic_and_records_all_versions() {
        let mut store = AssetStore::new(pack_descriptors(), runtime());
        let report = store.index_local().expect("index assets");
        assert_eq!(report.usable, 6);

        let first = build_semantic_index_from_asset_store(&store, &config()).expect("first index");
        let second =
            build_semantic_index_from_asset_store(&store, &config()).expect("second index");
        assert_eq!(first.metadata(), second.metadata());
        assert_eq!(first.metadata().retriever_version, "asset-semantic-v1");
        assert_eq!(first.metadata().embedding_model, "starter-semantic@1");
        assert_eq!(first.metadata().asset_compiler_version, "0.1.0");

        let result = first.search(&[1.0, 0.0, 0.0], 2).expect("search");
        assert_eq!(result.candidates[0].asset_id, "reaction.agree.01");
        assert!(result.candidates[0].asset_identity.is_some());

        let before_promotion = first.metadata().index_version.clone();
        store
            .preload_compatible()
            .expect("promote compatible assets");
        let after_promotion =
            build_semantic_index_from_asset_store(&store, &config()).expect("index after preload");
        assert_eq!(
            after_promotion.metadata().index_version,
            before_promotion,
            "L0 promotion must not mutate semantic snapshot identity"
        );
    }

    #[test]
    fn incompatible_assets_are_excluded_even_when_embedding_exists() {
        let dir = TestDir::new("incompatible");
        copy_descriptor("reaction-agree-01.json", dir.path());
        copy_descriptor("reaction-surprise-01.json", dir.path());

        let surprise_path = dir.path().join("reaction-surprise-01.json");
        let mut surprise: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(&surprise_path).expect("read")).expect("json");
        surprise["compatibility"]["voice_model"] =
            serde_json::Value::String("other-voice".to_owned());
        fs::write(
            &surprise_path,
            serde_json::to_vec_pretty(&surprise).expect("serialize"),
        )
        .expect("write");

        let mut store = AssetStore::new(dir.path(), runtime());
        let report = store.index_local().expect("index assets");
        assert_eq!(report.usable, 1);
        assert_eq!(report.incompatible, 1);

        let index = build_semantic_index_from_asset_store(&store, &config()).expect("index");
        let result = index.search(&[0.0, 1.0, 0.0], 8).expect("search");
        assert_eq!(result.candidates.len(), 1);
        assert_eq!(result.candidates[0].asset_id, "reaction.agree.01");
    }

    #[test]
    fn stale_embedding_model_is_excluded() {
        let dir = TestDir::new("stale-model");
        copy_descriptor("reaction-agree-01.json", dir.path());
        copy_descriptor("reaction-surprise-01.json", dir.path());

        let surprise_path = dir.path().join("reaction-surprise-01.json");
        let mut surprise: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(&surprise_path).expect("read")).expect("json");
        surprise["semantic_embedding"]["model_version"] = serde_json::Value::String("2".to_owned());
        fs::write(
            &surprise_path,
            serde_json::to_vec_pretty(&surprise).expect("serialize"),
        )
        .expect("write");

        let mut store = AssetStore::new(dir.path(), runtime());
        store.index_local().expect("index assets");

        let index = build_semantic_index_from_asset_store(&store, &config()).expect("index");
        let result = index.search(&[0.0, 1.0, 0.0], 8).expect("search");
        assert_eq!(result.candidates.len(), 1);
        assert_eq!(result.candidates[0].asset_id, "reaction.agree.01");
    }

    #[test]
    fn removed_assets_refresh_but_failed_reindex_keeps_previous_snapshot() {
        let dir = TestDir::new("refresh");
        copy_descriptor("reaction-agree-01.json", dir.path());
        copy_descriptor("reaction-agree-02.json", dir.path());

        let mut store = AssetStore::new(dir.path(), runtime());
        store.index_local().expect("initial index");
        let first = build_semantic_index_from_asset_store(&store, &config()).expect("first");
        let first_version = first.metadata().index_version.clone();
        assert_eq!(
            first
                .search(&[1.0, 0.0, 0.0], 8)
                .expect("search")
                .candidates
                .len(),
            2
        );

        fs::remove_file(dir.path().join("reaction-agree-02.json")).expect("remove descriptor");
        store.index_local().expect("refresh after removal");
        let second = build_semantic_index_from_asset_store(&store, &config()).expect("second");
        assert_ne!(second.metadata().index_version, first_version);
        assert_eq!(
            second
                .search(&[1.0, 0.0, 0.0], 8)
                .expect("search")
                .candidates
                .len(),
            1
        );

        let stable_version = second.metadata().index_version.clone();
        fs::write(dir.path().join("reaction-agree-01.json"), "{not-json")
            .expect("corrupt descriptor");
        assert!(store.index_local().is_err());
        let after_failed_reindex =
            build_semantic_index_from_asset_store(&store, &config()).expect("old snapshot");
        assert_eq!(
            after_failed_reindex.metadata().index_version,
            stable_version
        );
    }
}
