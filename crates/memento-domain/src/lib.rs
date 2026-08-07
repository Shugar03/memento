//! Memento RS core domain: tenant identity, memory chunks with mandatory
//! provenance, knowledge artifacts, and the shared error taxonomy (design D7).
//!
//! This crate is adapter-free: it defines what the system IS, not how it is
//! stored or served. Everything here is plain data + invariants.

/// Newtype over a UUID v7 identifier with `Display`, `FromStr`, `Serialize`
/// and `Deserialize`. Identifiers are machine-generated and content-free.
macro_rules! uuid_newtype {
    ($name:ident) => {
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
        pub struct $name(Uuid);

        impl $name {
            /// Generate a fresh UUID v7 identifier.
            pub fn new() -> Self {
                Self(Uuid::now_v7())
            }

            /// Wrap an existing UUID.
            pub const fn from_uuid(id: Uuid) -> Self {
                Self(id)
            }

            /// Borrow the inner UUID.
            pub const fn as_uuid(&self) -> &Uuid {
                &self.0
            }
        }

        impl Default for $name {
            fn default() -> Self {
                Self::new()
            }
        }

        impl std::fmt::Display for $name {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                self.0.fmt(f)
            }
        }

        impl std::str::FromStr for $name {
            type Err = uuid::Error;

            fn from_str(s: &str) -> Result<Self, Self::Err> {
                Ok(Self(Uuid::from_str(s)?))
            }
        }

        impl AsRef<Uuid> for $name {
            fn as_ref(&self) -> &Uuid {
                &self.0
            }
        }
    };
}

/// Newtype over a String identifier with `Display`, `FromStr`, `Serialize`
/// and `Deserialize`. Used for human-chosen identifiers (agent names, artifact
/// paths) that are not machine-generated.
macro_rules! string_newtype {
    ($name:ident) => {
        #[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
        pub struct $name(String);

        impl $name {
            /// Wrap a string identifier.
            pub fn new(id: impl Into<String>) -> Self {
                Self(id.into())
            }

            /// Borrow the inner string.
            pub fn as_str(&self) -> &str {
                &self.0
            }
        }

        impl Default for $name {
            fn default() -> Self {
                Self(String::new())
            }
        }

        impl std::fmt::Display for $name {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                self.0.fmt(f)
            }
        }

        impl std::str::FromStr for $name {
            type Err = std::convert::Infallible;

            fn from_str(s: &str) -> Result<Self, Self::Err> {
                Ok(Self(s.to_string()))
            }
        }

        impl AsRef<str> for $name {
            fn as_ref(&self) -> &str {
                &self.0
            }
        }
    };
}

pub mod chunk;
pub mod doc;
pub mod error;
pub mod feedback;
pub mod tenant;

pub use chunk::{MemoryChunk, Provenance, SourceKind};
pub use doc::{ArtifactKind, ChoreId, DocId, KnowledgeArtifact, KnowledgeArtifactId};
pub use error::DomainError;
pub use feedback::FeedbackId;
pub use tenant::{AgentId, TenantContext, TenantId, WorkspaceId};
