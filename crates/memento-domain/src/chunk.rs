//! Memory chunks and their mandatory provenance (REQ-MC-006).
//!
//! No chunk persists without complete provenance: `source`, `doc_id`,
//! `chunk_id`, `created_at`, `embedding_model_version`, `tenant_id`,
//! `workspace_id`, `agent_id`.

use crate::doc::DocId;
use crate::tenant::{AgentId, TenantId, WorkspaceId};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

// Chunk identifier — machine-generated (UUID v7).
uuid_newtype!(ChunkId);

/// Source kind of an ingested memory piece.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum SourceKind {
    /// Plain text (`memory.ingest_text`).
    Text,
    /// Markdown passthrough (fallback parser).
    Markdown,
    /// A normalized document; carries the original file extension
    /// (e.g. `"docx"`, `"pdf"`).
    Document(String),
}

/// Mandatory provenance stamped on every stored chunk (REQ-MC-006).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Provenance {
    pub source: SourceKind,
    pub doc_id: DocId,
    pub chunk_id: ChunkId,
    pub created_at: DateTime<Utc>,
    pub embedding_model_version: String,
    pub tenant_id: TenantId,
    pub workspace_id: WorkspaceId,
    pub agent_id: AgentId,
}

/// A persisted memory chunk (REQ-MC-001/004/006).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MemoryChunk {
    pub id: ChunkId,
    pub tenant_id: TenantId,
    pub workspace_id: WorkspaceId,
    pub agent_id: AgentId,
    pub doc_id: DocId,
    pub text: String,
    /// Embedding vector; `None` in `--no-embeddings` mode (REQ-MC-004).
    pub vector: Option<Vec<f32>>,
    pub created_at: DateTime<Utc>,
    pub provenance: Provenance,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::doc::ChoreId;
    use crate::feedback::FeedbackId;
    use crate::tenant::AgentId;
    use serde::de::DeserializeOwned;
    use std::fmt::{Debug, Display};
    use std::str::FromStr;

    #[test]
    fn provenance_complete_at_write() {
        // A chunk is written only with a fully populated provenance: every
        // REQ-MC-006 field is present and matches the execution context.
        let (tenant_id, workspace_id, agent_id) =
            (TenantId::new(), WorkspaceId::new(), AgentId::new("agent-a"));
        let chunk_id = ChunkId::new();
        let doc_id = DocId::new();

        let provenance = Provenance {
            source: SourceKind::Text,
            doc_id,
            chunk_id,
            created_at: Utc::now(),
            embedding_model_version: "multilingual-e5-small-v0.0.3".to_string(),
            tenant_id,
            workspace_id,
            agent_id: agent_id.clone(),
        };

        let chunk = MemoryChunk {
            id: chunk_id,
            tenant_id,
            workspace_id,
            agent_id,
            doc_id,
            text: "La memoria es la facultad de recordar.".to_string(),
            vector: Some(vec![0.1, 0.2, 0.3]),
            created_at: provenance.created_at,
            provenance: provenance.clone(),
        };

        // All provenance fields present and consistent with the chunk context.
        assert_eq!(chunk.provenance.source, SourceKind::Text);
        assert_eq!(chunk.provenance.doc_id, chunk.doc_id);
        assert_eq!(chunk.provenance.chunk_id, chunk.id);
        assert_eq!(chunk.provenance.created_at, chunk.created_at);
        assert!(!chunk.provenance.embedding_model_version.is_empty());
        assert_eq!(chunk.provenance.tenant_id, chunk.tenant_id);
        assert_eq!(chunk.provenance.workspace_id, chunk.workspace_id);
        assert_eq!(chunk.provenance.agent_id, chunk.agent_id);
    }

    #[test]
    fn newtypes_round_trip_serde() {
        // Every newtype survives serde JSON and Display/FromStr round trips.
        fn round_trip<T>(value: T)
        where
            T: Serialize + DeserializeOwned + Display + FromStr + PartialEq + Debug,
            T::Err: Debug,
        {
            let json = serde_json::to_string(&value).expect("serialize");
            let back: T = serde_json::from_str(&json).expect("deserialize");
            assert_eq!(value, back, "serde round trip");

            let s = value.to_string();
            let parsed: T = s.parse().expect("parse display");
            assert_eq!(value, parsed, "display/fromstr round trip");
        }

        round_trip(ChunkId::new());
        round_trip(DocId::new());
        round_trip(ChoreId::new());
        round_trip(FeedbackId::new());
        round_trip(TenantId::new());
        round_trip(WorkspaceId::new());
        round_trip(AgentId::new("agent-a"));
        round_trip(crate::doc::KnowledgeArtifactId::new("src/main.rs"));
    }
}
