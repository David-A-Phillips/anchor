//! Shared foundation: the vocabulary, data model and contracts every other module builds on.
//!
//! Nothing in here depends on a specific subsystem. The types in this module are the stable seams
//! between subsystems:
//!
//! * [`model`] — units, assertions and graph deltas (ingestion → store → in-memory graph → sync);
//! * [`keys`] — canonical natural keys and alias normalisation (entity identity across sources);
//! * [`kinds`] — node/edge/source/unit vocabularies and [`TrustLevel`];
//! * [`hash`], [`hlc`] — content addressing and versioning;
//! * [`db`] — the shared SQLite handle and per-module migrations;
//! * [`llm`], [`usage`], [`embedding`], [`query`] — contracts for models, the token ledger,
//!   embeddings and query understanding;
//! * [`net`] — shared HTTP client construction.

mod macros;

pub mod attrs;
pub mod db;
pub mod embedding;
pub mod error;
pub mod hash;
pub mod hlc;
pub mod ids;
pub mod keys;
pub mod kinds;
pub mod llm;
pub mod model;
pub mod net;
pub mod query;
pub mod time;
pub mod usage;

// Available to every module that declares its own closed vocabulary.
#[allow(unused_imports)]
pub(crate) use macros::{opaque_debug, string_enum};

pub use db::{Db, Migration, MigrationSet};
pub use embedding::{EmbedKind, Embedder};
pub use error::{Error, Result};
pub use hash::{ContentHash, canonical_json};
pub use hlc::{Hlc, HlcClock};
pub use ids::{ChunkId, DeviceId, EdgeId, NodeId, NodeKey, SourceId, UnitId, UnitKey};
pub use kinds::{ChunkKind, EdgeKind, NodeKind, SourceKind, SymbolKind, TrustLevel, UnitKind};
pub use llm::{
    CallPurpose, ChatMessage, Completion, CompletionRequest, FinishReason, LanguageModel, Locality,
    ModelError, ModelInfo, ModelTier, ResponseFormat, Role, TokenUsage,
};
pub use model::{
    Attrs, Chunk, EdgeAssertion, GraphDelta, GraphEdge, GraphNode, Lookup, LookupScope,
    NodeAssertion, NodeRef, SourceDescriptor, Unit, UnitMeta, UnitVersion,
};
pub use query::{ClassifierKind, Complexity, EntityMention, IntentKind, QueryIntent};
pub use time::now_ms;
pub use usage::{MemoryUsageSink, NullUsageSink, UsageRecord, UsageSink};
