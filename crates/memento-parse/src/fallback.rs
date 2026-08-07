//! Fallback parsers (T-031): markdown and plain-text passthrough.
//!
//! Used when the document is already in a trivial format (`SourceKind::Text`,
//! `SourceKind::Markdown`, or a `.md`/`.txt` file extension) — no subprocess
//! is ever spawned for these. The contract is a byte-faithful UTF-8
//! passthrough (a leading UTF-8 BOM is stripped) so chunking operates on
//! exactly what was ingested.

use memento_domain::{DomainError, SourceKind};
use memento_ports::parse::ParsedDocument;
use serde_json::json;

/// File extensions handled by the fallback passthrough parser (never
/// spawned through anydoc). md/txt keep their original `Document` source
/// kind so provenance records what was ingested.
pub const FALLBACK_EXTENSIONS: &[&str] = &["md", "markdown", "txt", "text"];

/// The fallback normalization boundary.
#[derive(Debug, Clone, Default)]
pub struct FallbackParser;

impl FallbackParser {
    /// Passthrough-normalize a blob, preserving the ingested `source_kind`.
    ///
    /// # Errors
    ///
    /// * `Parse` — blob is not valid UTF-8 (stage-named, REQ-MC-007).
    pub fn parse(
        &self,
        blob: &[u8],
        source_kind: SourceKind,
    ) -> Result<ParsedDocument, DomainError> {
        let text = std::str::from_utf8(blob).map_err(|_| DomainError::Parse {
            message: format!(
                "fallback {} parse failed: blob is not valid UTF-8",
                kind_name(&source_kind)
            ),
        })?;
        // Strip a UTF-8 BOM if present: BOM is not markdown content.
        let text = text.strip_prefix('\u{feff}').unwrap_or(text);

        let parser = kind_name(&source_kind);
        Ok(ParsedDocument {
            markdown: text.to_string(),
            source_kind,
            metadata: json!({
                "parser": format!("fallback:{parser}"),
                "source_bytes": blob.len(),
            }),
        })
    }
}

fn kind_name(kind: &SourceKind) -> &'static str {
    match kind {
        SourceKind::Text => "text",
        SourceKind::Markdown => "markdown",
        SourceKind::Document(ext) if ext.eq_ignore_ascii_case("md") => "markdown",
        SourceKind::Document(ext) if ext.eq_ignore_ascii_case("txt") => "text",
        SourceKind::Document(_) => "document",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use memento_domain::SourceKind;

    #[test]
    fn text_passthrough_is_byte_faithful() {
        let parser = FallbackParser;
        let blob = b"La memoria es la facultad de recordar.";
        let doc = parser.parse(blob, SourceKind::Text).unwrap();
        assert_eq!(doc.markdown, "La memoria es la facultad de recordar.");
        assert_eq!(doc.source_kind, SourceKind::Text);
        assert_eq!(doc.metadata["parser"], "fallback:text");
    }

    #[test]
    fn markdown_passthrough_strips_bom() {
        let parser = FallbackParser;
        let mut blob = vec![0xEF, 0xBB, 0xBF]; // UTF-8 BOM
        blob.extend_from_slice(b"# T\xc3\xadtulo\n\nContenido.");
        let doc = parser.parse(&blob, SourceKind::Markdown).unwrap();
        assert_eq!(doc.markdown, "# Título\n\nContenido.");
        assert_eq!(doc.metadata["parser"], "fallback:markdown");
    }

    #[test]
    fn invalid_utf8_rejected_with_parse_code() {
        let parser = FallbackParser;
        let err = parser
            .parse(&[0xFF, 0xFE, 0x00], SourceKind::Markdown)
            .unwrap_err();
        assert_eq!(err.code(), "PARSE");
        assert!(err.to_string().contains("UTF-8"));
    }

    #[test]
    fn document_kind_keeps_original_source() {
        let parser = FallbackParser;
        let doc = parser
            .parse(b"# md file", SourceKind::Document("md".into()))
            .unwrap();
        assert_eq!(doc.source_kind, SourceKind::Document("md".into()));
        assert_eq!(doc.metadata["parser"], "fallback:markdown");
    }
}
