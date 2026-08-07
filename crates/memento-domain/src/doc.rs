//! Document, chore, and knowledge-artifact identities plus the
//! `KnowledgeArtifact` type for the code-knowledge layer (REQ-CK-*).

use serde::{Deserialize, Serialize};
use serde_json::Value;
use uuid::Uuid;

// Document identifier — machine-generated (UUID v7). Auto-generated at ingest
// when the caller does not provide one (REQ-MC-001).
uuid_newtype!(DocId);
// Chore identifier — machine-generated (UUID v7). Makes ingest and
// maintenance operations observable (REQ-MC-007).
uuid_newtype!(ChoreId);
// Knowledge-artifact identifier — human-meaningful (relative path or symbol
// name, e.g. "src/main.rs").
string_newtype!(KnowledgeArtifactId);

/// Layer of a code-knowledge artifact (design layers L1..L4).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ArtifactKind {
    /// L1: okf-rs Markdown/YAML bundle (source of truth).
    Bundle,
    /// L2: symbol index entry.
    Symbol,
    /// L3: relationship graph (`graph.json`).
    Graph,
    /// L4: architectural summary (`summary.md`).
    Summary,
}

/// A code-knowledge artifact, shaped per bundle layer (design `KnowledgeArtifact`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct KnowledgeArtifact {
    pub project_id: String,
    pub artifact_id: KnowledgeArtifactId,
    pub kind: ArtifactKind,
    pub content: Value,
}
