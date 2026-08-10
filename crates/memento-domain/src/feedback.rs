//! Feedback identity (REQ-ML-001). Feedback records are persisted with
//! attribution; the id is machine-generated.

use serde::{Deserialize, Serialize};
use uuid::Uuid;

// Feedback identifier — machine-generated (UUID v7).
uuid_newtype!(FeedbackId);
