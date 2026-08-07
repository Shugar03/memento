//! memento-parse — document normalization and deterministic chunking.
//!
//! Two responsibilities (T-031/T-032):
//!
//! * **Normalization boundary** (REQ-MC-002): document blobs become
//!   Markdown through the anydoc subprocess (see [`anydoc`]) with fallback
//!   parsers for md/txt (see [`fallback`]). [`ParseService`] implements
//!   `memento_ports::ParsePort` and routes by `SourceKind`.
//! * **Deterministic chunking** (REQ-MC-003): [`chunk`] splits normalized
//!   text into 256-300 token chunks with 10-15% overlap using a Spanish
//!   tokenizer, truncation off.

pub mod anydoc;
pub mod fallback;

use anydoc::{ANYDOC_EXTENSIONS, AnydocClient, AnydocConfig};
use async_trait::async_trait;
use fallback::FALLBACK_EXTENSIONS;
use memento_domain::{DomainError, SourceKind};
use memento_ports::parse::{ParsePort, ParsedDocument};
use serde_json::json;

/// The routing + fallback normalization boundary (T-031).
#[derive(Debug, Clone)]
pub struct ParseService {
    anydoc: AnydocClient,
    fallback: fallback::FallbackParser,
}

impl ParseService {
    /// Build the service with an explicit subprocess configuration (tests
    /// inject a fake binary; production passes `AnydocConfig` with the
    /// resolved command).
    pub fn new(anydoc: AnydocConfig) -> Self {
        Self {
            anydoc: AnydocClient::new(anydoc),
            fallback: fallback::FallbackParser,
        }
    }

    /// Build the service with the resolved anydoc command (`anydoc` on
    /// PATH, else pinned `npx --yes @firecrawl/anydoc@0.1.7`) and
    /// production limits (60 s timeout, 50 MiB stdout cap).
    ///
    /// # Errors
    ///
    /// * `Parse` — no anydoc converter could be resolved on this host.
    pub fn auto(staging_dir: std::path::PathBuf) -> Result<Self, DomainError> {
        let command = anydoc::detect_anydoc_command()?;
        Ok(Self::new(AnydocConfig {
            command,
            timeout: anydoc::DEFAULT_TIMEOUT,
            stdout_limit: anydoc::DEFAULT_STDOUT_LIMIT,
            staging_dir,
        }))
    }
}

/// Normalize an extension for classification: lowercase, tolerate a single
/// leading dot. Unsupported extensions never reach the subprocess (the
/// allowlist in `anydoc::validate_ext` is the argv gate).
fn classify_ext(ext: &str) -> String {
    ext.strip_prefix('.').unwrap_or(ext).to_ascii_lowercase()
}

#[async_trait]
impl ParsePort for ParseService {
    async fn parse(&self, blob: &[u8], hint: SourceKind) -> Result<ParsedDocument, DomainError> {
        match hint {
            // Raw text and markdown never touch the subprocess.
            SourceKind::Text => self.fallback.parse(blob, SourceKind::Text),
            SourceKind::Markdown => self.fallback.parse(blob, SourceKind::Markdown),
            SourceKind::Document(ext) => {
                let ext = classify_ext(&ext);
                // The argv gate fires before ANY routing: the extension is
                // the only user-controlled fragment that can reach the
                // subprocess, so every layer surfaces the same stable code
                // (SUBPROCESS_ARGV_INVALID) for the same input (T-030).
                anydoc::validate_ext(&ext)?;
                if FALLBACK_EXTENSIONS.contains(&ext.as_str()) {
                    // Trivial formats: direct passthrough; provenance keeps
                    // the original source kind (Document).
                    return self.fallback.parse(blob, SourceKind::Document(ext));
                }
                if !ANYDOC_EXTENSIONS.contains(&ext.as_str()) {
                    return Err(DomainError::InvalidInput {
                        message: format!("unsupported document format: '{ext}'"),
                    });
                }
                // Subprocess errors propagate with their stable codes
                // (timeout/overflow/argv) — the RED boundary contract.
                let converted = self.anydoc.convert(blob, &ext).await?;
                Ok(ParsedDocument {
                    markdown: converted.markdown,
                    source_kind: SourceKind::Document(ext.clone()),
                    metadata: json!({
                        "parser": "anydoc",
                        "format": ext,
                        "source_bytes": converted.input_bytes,
                    }),
                })
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classify_ext_normalizes() {
        assert_eq!(classify_ext("DOCX"), "docx");
        assert_eq!(classify_ext(".MD"), "md");
        assert_eq!(classify_ext("txt"), "txt");
    }

    #[test]
    fn routing_tables_are_disjoint() {
        // Fallback formats must never route to the subprocess.
        for ext in FALLBACK_EXTENSIONS {
            assert!(
                !ANYDOC_EXTENSIONS.contains(ext),
                "fallback ext {ext} must not route to the subprocess"
            );
        }
        // The anydoc table carries the documented format set.
        assert!(
            ANYDOC_EXTENSIONS.contains(&"pdf")
                && ANYDOC_EXTENSIONS.contains(&"docx")
                && ANYDOC_EXTENSIONS.contains(&"xlsx")
        );
    }
}
