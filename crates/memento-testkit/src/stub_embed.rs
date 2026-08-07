//! Deterministic embedding fake (design D2 — no ONNX runtime needed).
//!
//! [`deterministic_embed`] maps a text to a fixed-dimension vector via
//! hash-bucketed coordinates: the same text always yields the same vector
//! (test reproducibility), and different texts yield different vectors with
//! high probability (enough to rank nearest neighbors in tests).

use async_trait::async_trait;
use memento_domain::DomainError;
use memento_ports::EmbedPort;

/// Deterministic hash-bucketed embedding.
///
/// Each whitespace token is FNV-1a hashed and its hash is scattered onto a
/// handful of coordinates (splitmix64-derived indices, sign from the hash).
/// This gives the fake two properties tests rely on:
///
/// * **Determinism** — the same text always yields bit-identical vectors.
/// * **Similarity structure** — texts sharing tokens land on overlapping
///   coordinates with the same signs, so cosine/L2 nearest-neighbor ranking
///   behaves like a real embedding (needed by ranking tests).
pub fn deterministic_embed(text: &str, dim: usize) -> Vec<f32> {
    let mut out = vec![0.0f32; dim];

    // Distinct tokens get scattered onto distinct-but-consistent coordinates.
    const SCATTER: usize = 8;
    for token in text.split_whitespace() {
        let mut h: u64 = 0xcbf2_9ce4_8422_2325; // FNV-1a
        for byte in token.to_lowercase().as_bytes() {
            h ^= u64::from(*byte);
            h = h.wrapping_mul(0x0000_0100_0000_01b3);
        }
        for k in 0..SCATTER {
            let mut h2 = h ^ (k as u64).wrapping_mul(0x9e37_79b9_7f4a_7c15);
            h2 ^= h2 >> 33;
            h2 = h2.wrapping_mul(0xff51_afd7_ed55_8ccd);
            h2 ^= h2 >> 33;
            let idx = (h2 % dim as u64) as usize;
            let sign = if (h2 >> 32) & 1 == 0 { 1.0 } else { -1.0 };
            out[idx] += sign;
        }
    }

    let norm = out.iter().map(|v| v * v).sum::<f32>().sqrt();
    if norm > 0.0 {
        for v in &mut out {
            *v /= norm;
        }
    }
    out
}

/// [`EmbedPort`] fake backed by [`deterministic_embed`].
#[derive(Debug, Clone)]
pub struct StubEmbedPort {
    /// Vector dimension (defaults to the E5-small 384 used in production).
    pub dim: usize,
}

impl Default for StubEmbedPort {
    fn default() -> Self {
        Self { dim: 384 }
    }
}

#[async_trait]
impl EmbedPort for StubEmbedPort {
    async fn embed(&self, texts: &[&str]) -> Result<Vec<Vec<f32>>, DomainError> {
        Ok(texts
            .iter()
            .map(|t| deterministic_embed(t, self.dim))
            .collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fake_embed_is_deterministic() {
        // Same text → same vector, across calls.
        let a = deterministic_embed("la memoria es un río", 384);
        let b = deterministic_embed("la memoria es un río", 384);
        assert_eq!(a, b);

        // Different texts → different vectors.
        let c = deterministic_embed("la memoria es un lago", 384);
        assert_ne!(a, c, "different text must hash to a different vector");

        // Dimension contract: 384 (E5-small), matching the storage schema.
        assert_eq!(a.len(), 384);
    }

    #[test]
    fn fake_embed_is_normalized() {
        let v = deterministic_embed("vector unitario", 64);
        let norm = v.iter().map(|x| x * x).sum::<f32>().sqrt();
        assert!((norm - 1.0).abs() < 1e-3, "expected unit norm, got {norm}");
    }

    #[tokio::test]
    async fn stub_embed_port_lines_up_with_input() {
        let port = StubEmbedPort::default();
        let out = port
            .embed(&["uno", "dos", "tres"])
            .await
            .expect("stub never fails");
        assert_eq!(out.len(), 3);
        assert_eq!(out[0], deterministic_embed("uno", 384));
        assert_eq!(out[2], deterministic_embed("tres", 384));
    }
}
