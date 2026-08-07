//! memento-i18n — bilingual (ES-first, EN-fallback) user-facing strings and
//! error rendering (REQ-MS-004, REQ-CL-004).
//!
//! All user-facing text (MCP tool descriptions, error messages, CLI help)
//! lives in [`strings`]; [`error_render`] renders `DomainError` per locale.

pub mod error_render;
pub mod strings;

pub use error_render::{format_error, format_error_json};
pub use strings::{Locale, StringKey, en, es, lookup};

use memento_domain::DomainError;

/// Bilingual string provider. ES is the primary locale; EN is the fallback.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct I18n {
    locale: Locale,
}

impl I18n {
    /// Load the string table for `locale`.
    pub fn load(locale: Locale) -> Self {
        Self { locale }
    }

    /// Alias of [`I18n::load`].
    pub fn new(locale: Locale) -> Self {
        Self::load(locale)
    }

    /// The configured locale.
    pub fn locale(&self) -> Locale {
        self.locale
    }

    /// Resolve a key to the string for the configured locale.
    pub fn t(&self, key: StringKey) -> &'static str {
        lookup(key, self.locale)
    }

    /// Error message for the configured locale (ES primary, EN fallback).
    pub fn format_error(&self, err: &DomainError) -> String {
        format_error(err, self.locale)
    }
}

impl Default for I18n {
    fn default() -> Self {
        Self::load(Locale::Es)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use memento_domain::{ChunkId, DomainError};

    #[test]
    fn es_strings_primary() {
        let i18n = I18n::load(Locale::Es);

        // ES strings come from the ES table, not the EN table.
        assert_eq!(
            i18n.t(StringKey::McpToolSearchDesc),
            es(StringKey::McpToolSearchDesc)
        );
        assert_ne!(
            i18n.t(StringKey::McpToolSearchDesc),
            en(StringKey::McpToolSearchDesc)
        );

        // Spot-check actual Spanish content across the three categories.
        assert!(
            i18n.t(StringKey::ErrTenantRequired)
                .contains("contexto de tenant")
        );
        assert!(i18n.t(StringKey::CliHelpSearch).contains("Busca"));
        assert!(i18n.t(StringKey::CodeToolGraphDumpDesc).contains("nodos"));

        // ES is the default locale (ES-first).
        assert_eq!(Locale::default(), Locale::Es);
        assert_eq!(I18n::default().locale(), Locale::Es);
    }

    #[test]
    fn en_fallback() {
        let i18n = I18n::load(Locale::En);

        // EN strings come from the EN table (the fallback language).
        assert_eq!(
            i18n.t(StringKey::McpToolSearchDesc),
            en(StringKey::McpToolSearchDesc)
        );
        assert_ne!(
            i18n.t(StringKey::McpToolSearchDesc),
            es(StringKey::McpToolSearchDesc)
        );
        assert!(
            i18n.t(StringKey::ErrTenantRequired)
                .contains("tenant context")
        );
        assert_eq!(i18n.locale(), Locale::En);
    }

    #[test]
    fn tables_cover_same_keys() {
        // Parity: every key exists in both tables (no missing fallback).
        for key in StringKey::ALL {
            assert!(!es(key).is_empty(), "ES missing {key:?}");
            assert!(!en(key).is_empty(), "EN missing {key:?}");
            assert_ne!(es(key), en(key), "ES and EN identical for {key:?}");
        }
    }

    #[test]
    fn error_render_es() {
        let msg = format_error(&DomainError::TenantRequired, Locale::Es);
        assert_eq!(msg, es(StringKey::ErrTenantRequired));
        assert!(msg.contains("contexto de tenant"), "ES message, got: {msg}");

        let msg = format_error(
            &DomainError::ChunkNotFound { id: ChunkId::new() },
            Locale::Es,
        );
        assert_eq!(msg, es(StringKey::ErrChunkNotFound));
    }

    #[test]
    fn error_render_en() {
        let msg = format_error(&DomainError::TenantRequired, Locale::En);
        assert_eq!(msg, en(StringKey::ErrTenantRequired));
        assert!(msg.contains("tenant context"), "EN message, got: {msg}");
    }

    #[test]
    fn error_render_is_deterministic() {
        // Same (err, locale) -> same string, every time.
        let err = DomainError::QuotaExceeded {
            message: "x".into(),
        };
        for locale in [Locale::Es, Locale::En] {
            let a = format_error(&err, locale);
            let b = format_error(&err, locale);
            assert_eq!(a, b);
        }
    }

    #[test]
    fn structured_error_json_carries_code() {
        let err = DomainError::WorkspaceRequired;
        let v = format_error_json(&err, Locale::Es);
        assert_eq!(v["code"], "WORKSPACE_REQUIRED");
        assert_eq!(v["exit_code"], 15);
        assert_eq!(v["message"], es(StringKey::ErrWorkspaceRequired));
        assert!(!v["detail"].as_str().unwrap().is_empty());
    }

    #[test]
    fn snapshot_mcp_tool_descriptions() {
        // Golden strings for the 7 memory tools (REQ-MS-002). Changing a
        // description intentionally requires updating both the ES/EN tables
        // and these expects.
        expect_test::expect![[r#"Busca en la memoria del workspace: búsqueda de texto completo y, si está habilitada, búsqueda híbrida con RRF."#]]
            .assert_eq(es(StringKey::McpToolSearchDesc));
        expect_test::expect![[
            r#"Ingresa texto plano y lo convierte en fragmentos de memoria buscables."#
        ]]
        .assert_eq(es(StringKey::McpToolIngestTextDesc));
        expect_test::expect![[
            r#"Ingresa un documento (14 formatos), lo normaliza a Markdown y lo indexa en memoria."#
        ]]
        .assert_eq(es(StringKey::McpToolIngestDocumentDesc));
        expect_test::expect![[
            r#"Obtiene un fragmento de memoria por su id, con su procedencia completa."#
        ]]
        .assert_eq(es(StringKey::McpToolGetChunkDesc));
        expect_test::expect![[
            r#"Registra retroalimentación (relevante / irrelevante) sobre un fragmento."#
        ]]
        .assert_eq(es(StringKey::McpToolFeedbackDesc));
        expect_test::expect![[
            r#"Elimina de forma permanente fragmentos, documentos, workspaces o el tenant."#
        ]]
        .assert_eq(es(StringKey::McpToolDeleteDesc));
        expect_test::expect![[r#"Selecciona los fragmentos que mejor caben en un presupuesto de tokens para contexto."#]]
            .assert_eq(es(StringKey::McpToolContextFitDesc));
    }
}
