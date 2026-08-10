//! Bilingual string tables: ES primary, EN fallback (REQ-MS-004, REQ-CL-004).
//!
//! Every user-facing string (MCP tool descriptions, error messages, CLI help)
//! is defined here. ES is the default locale; EN is available as the
//! fallback language. Identifiers and code stay English — only the strings
//! shown to users live in this table.

use serde::{Deserialize, Serialize};

/// Locale of the string tables. ES is the default (ES-first).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
pub enum Locale {
    #[default]
    Es,
    En,
}

impl Locale {
    pub fn as_str(&self) -> &'static str {
        match self {
            Locale::Es => "es",
            Locale::En => "en",
        }
    }
}

/// Stable key for every user-facing string. Keys MUST NOT change between
/// releases; string VALUES live in the ES and EN tables below.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum StringKey {
    // MCP memory tools (REQ-MS-002/004).
    McpToolSearchDesc,
    McpToolIngestTextDesc,
    McpToolIngestDocumentDesc,
    McpToolGetChunkDesc,
    McpToolFeedbackDesc,
    McpToolDeleteDesc,
    McpToolContextFitDesc,
    // Code tools (T-073, 8 tools).
    CodeToolProjectOverviewDesc,
    CodeToolSymbolLookupDesc,
    CodeToolCallersOfDesc,
    CodeToolCalleesOfDesc,
    CodeToolImpactDesc,
    CodeToolDependenciesDesc,
    CodeToolSearchDesc,
    CodeToolGraphDumpDesc,
    // Error messages (one per DomainError code, T-011).
    ErrTenantRequired,
    ErrTenantForbidden,
    ErrQuotaExceeded,
    ErrEmbeddingModelMismatch,
    ErrTopKExceeded,
    ErrWorkspaceRequired,
    ErrChunkNotFound,
    ErrResourceExhausted,
    ErrCapacityExceeded,
    ErrInternal,
    ErrNotFound,
    ErrInvalidInput,
    ErrAlreadyExists,
    ErrAuthFailed,
    ErrIo,
    ErrParse,
    ErrEmbeddingFailed,
    ErrBackupCorrupt,
    ErrBackupVersion,
    ErrSubprocessTimeout,
    ErrSubprocessStdoutOverflow,
    ErrSubprocessArgvInvalid,
    // CLI help (REQ-CL-004).
    CliHelpTenantCreate,
    CliHelpIngest,
    CliHelpSearch,
    CliHelpRotateToken,
    CliHelpDelete,
    CliHelpContextFit,
    CliHelpHealth,
    CliHelpCodeIndex,
}

impl StringKey {
    /// Every key, so tests can prove ES/EN parity.
    pub const ALL: [StringKey; 45] = [
        StringKey::McpToolSearchDesc,
        StringKey::McpToolIngestTextDesc,
        StringKey::McpToolIngestDocumentDesc,
        StringKey::McpToolGetChunkDesc,
        StringKey::McpToolFeedbackDesc,
        StringKey::McpToolDeleteDesc,
        StringKey::McpToolContextFitDesc,
        StringKey::CodeToolProjectOverviewDesc,
        StringKey::CodeToolSymbolLookupDesc,
        StringKey::CodeToolCallersOfDesc,
        StringKey::CodeToolCalleesOfDesc,
        StringKey::CodeToolImpactDesc,
        StringKey::CodeToolDependenciesDesc,
        StringKey::CodeToolSearchDesc,
        StringKey::CodeToolGraphDumpDesc,
        StringKey::ErrTenantRequired,
        StringKey::ErrTenantForbidden,
        StringKey::ErrQuotaExceeded,
        StringKey::ErrEmbeddingModelMismatch,
        StringKey::ErrTopKExceeded,
        StringKey::ErrWorkspaceRequired,
        StringKey::ErrChunkNotFound,
        StringKey::ErrResourceExhausted,
        StringKey::ErrCapacityExceeded,
        StringKey::ErrInternal,
        StringKey::ErrNotFound,
        StringKey::ErrInvalidInput,
        StringKey::ErrAlreadyExists,
        StringKey::ErrAuthFailed,
        StringKey::ErrIo,
        StringKey::ErrParse,
        StringKey::ErrEmbeddingFailed,
        StringKey::ErrBackupCorrupt,
        StringKey::ErrBackupVersion,
        StringKey::ErrSubprocessTimeout,
        StringKey::ErrSubprocessStdoutOverflow,
        StringKey::ErrSubprocessArgvInvalid,
        StringKey::CliHelpTenantCreate,
        StringKey::CliHelpIngest,
        StringKey::CliHelpSearch,
        StringKey::CliHelpRotateToken,
        StringKey::CliHelpDelete,
        StringKey::CliHelpContextFit,
        StringKey::CliHelpHealth,
        StringKey::CliHelpCodeIndex,
    ];
}

/// Spanish (primary) table.
pub fn es(key: StringKey) -> &'static str {
    match key {
        // MCP memory tools.
        StringKey::McpToolSearchDesc => {
            "Busca en la memoria del workspace: búsqueda de texto completo y, si está habilitada, búsqueda híbrida con RRF."
        }
        StringKey::McpToolIngestTextDesc => {
            "Ingresa texto plano y lo convierte en fragmentos de memoria buscables."
        }
        StringKey::McpToolIngestDocumentDesc => {
            "Ingresa un documento (14 formatos), lo normaliza a Markdown y lo indexa en memoria."
        }
        StringKey::McpToolGetChunkDesc => {
            "Obtiene un fragmento de memoria por su id, con su procedencia completa."
        }
        StringKey::McpToolFeedbackDesc => {
            "Registra retroalimentación (relevante / irrelevante) sobre un fragmento."
        }
        StringKey::McpToolDeleteDesc => {
            "Elimina de forma permanente fragmentos, documentos, workspaces o el tenant."
        }
        StringKey::McpToolContextFitDesc => {
            "Selecciona los fragmentos que mejor caben en un presupuesto de tokens para contexto."
        }
        // Code tools.
        StringKey::CodeToolProjectOverviewDesc => "Resumen arquitectónico del proyecto (capa L4).",
        StringKey::CodeToolSymbolLookupDesc => {
            "Busca un símbolo (función, tipo, constante) en el índice."
        }
        StringKey::CodeToolCallersOfDesc => "Quiénes llaman a un símbolo (hasta profundidad 2).",
        StringKey::CodeToolCalleesOfDesc => "A quién llama un símbolo (hasta profundidad 2).",
        StringKey::CodeToolImpactDesc => {
            "Alcance de impacto inverso: qué se rompería si cambia un símbolo."
        }
        StringKey::CodeToolDependenciesDesc => "Dependencias del proyecto y detección de ciclos.",
        StringKey::CodeToolSearchDesc => "Busca código por símbolo o texto (literal y semántico).",
        StringKey::CodeToolGraphDumpDesc => "Grafo canónico {nodos, aristas} del proyecto.",
        // Error messages.
        StringKey::ErrTenantRequired => "No hay contexto de tenant vinculado a esta ejecución.",
        StringKey::ErrTenantForbidden => "La operación intenta cruzar el límite del tenant.",
        StringKey::ErrQuotaExceeded => "Se superó la cuota del tenant.",
        StringKey::ErrEmbeddingModelMismatch => "Conflicto de versión del modelo de embeddings.",
        StringKey::ErrTopKExceeded => "El límite de búsqueda (top_k) supera el máximo permitido.",
        StringKey::ErrWorkspaceRequired => "La búsqueda requiere un workspace.",
        StringKey::ErrChunkNotFound => "El fragmento solicitado no existe.",
        StringKey::ErrResourceExhausted => "Recursos agotados.",
        StringKey::ErrCapacityExceeded => "Almacenamiento de respaldos lleno.",
        StringKey::ErrInternal => "Error interno.",
        StringKey::ErrNotFound => "No encontrado.",
        StringKey::ErrInvalidInput => "Entrada no válida.",
        StringKey::ErrAlreadyExists => "Ya existe un elemento duplicado.",
        StringKey::ErrAuthFailed => "Falló la autenticación.",
        StringKey::ErrIo => "Error de entrada/salida.",
        StringKey::ErrParse => "No se pudo analizar el documento.",
        StringKey::ErrEmbeddingFailed => "Falló la generación de embeddings.",
        StringKey::ErrBackupCorrupt => {
            "El respaldo está corrupto (checksum o descifrado fallaron)."
        }
        StringKey::ErrBackupVersion => "La versión del esquema del respaldo no coincide.",
        StringKey::ErrSubprocessTimeout => "El proceso auxiliar superó los 60 segundos.",
        StringKey::ErrSubprocessStdoutOverflow => {
            "La salida del proceso auxiliar superó los 50 MB."
        }
        StringKey::ErrSubprocessArgvInvalid => {
            "Argumentos del proceso auxiliar inválidos (metacaracteres en la ruta)."
        }
        // CLI help.
        StringKey::CliHelpTenantCreate => {
            "Crea un tenant y muestra el token de acceso una sola vez."
        }
        StringKey::CliHelpIngest => "Ingresa texto o documentos en memoria.",
        StringKey::CliHelpSearch => "Busca en la memoria del workspace.",
        StringKey::CliHelpRotateToken => {
            "Rota el token del tenant (requiere reiniciar el proceso)."
        }
        StringKey::CliHelpDelete => "Elimina memoria de forma permanente.",
        StringKey::CliHelpContextFit => "Prepara contexto con los fragmentos más relevantes.",
        StringKey::CliHelpHealth => "Verifica el estado del servicio.",
        StringKey::CliHelpCodeIndex => "Indexa un proyecto de código (Rust/Python).",
    }
}

/// English (fallback) table.
pub fn en(key: StringKey) -> &'static str {
    match key {
        // MCP memory tools.
        StringKey::McpToolSearchDesc => {
            "Search workspace memory: full-text search, plus hybrid RRF when enabled."
        }
        StringKey::McpToolIngestTextDesc => {
            "Ingest plain text and turn it into searchable memory chunks."
        }
        StringKey::McpToolIngestDocumentDesc => {
            "Ingest a document (14 formats), normalize it to Markdown, and index it into memory."
        }
        StringKey::McpToolGetChunkDesc => "Fetch a memory chunk by id with its full provenance.",
        StringKey::McpToolFeedbackDesc => "Record feedback (relevant / irrelevant) about a chunk.",
        StringKey::McpToolDeleteDesc => {
            "Permanently delete chunks, documents, workspaces, or the tenant."
        }
        StringKey::McpToolContextFitDesc => {
            "Pick the best-fitting chunks for a token budget (context packing)."
        }
        // Code tools.
        StringKey::CodeToolProjectOverviewDesc => "Architectural summary of the project (L4).",
        StringKey::CodeToolSymbolLookupDesc => {
            "Look up a symbol (function, type, constant) in the index."
        }
        StringKey::CodeToolCallersOfDesc => "Who calls a symbol (up to depth 2).",
        StringKey::CodeToolCalleesOfDesc => "What a symbol calls (up to depth 2).",
        StringKey::CodeToolImpactDesc => "Reverse impact: what would break if a symbol changes.",
        StringKey::CodeToolDependenciesDesc => "Project dependencies and cycle detection.",
        StringKey::CodeToolSearchDesc => "Search code by symbol or text (literal and semantic).",
        StringKey::CodeToolGraphDumpDesc => "Canonical {nodes, edges} graph of the project.",
        // Error messages.
        StringKey::ErrTenantRequired => "No tenant context is bound to this execution.",
        StringKey::ErrTenantForbidden => "Operation attempts to cross the tenant boundary.",
        StringKey::ErrQuotaExceeded => "Per-tenant quota exceeded.",
        StringKey::ErrEmbeddingModelMismatch => "Embedding model version conflict.",
        StringKey::ErrTopKExceeded => "Search limit (top_k) exceeds the allowed maximum.",
        StringKey::ErrWorkspaceRequired => "Search requires a workspace.",
        StringKey::ErrChunkNotFound => "The requested chunk does not exist.",
        StringKey::ErrResourceExhausted => "Resources exhausted.",
        StringKey::ErrCapacityExceeded => "Backup storage is full.",
        StringKey::ErrInternal => "Internal error.",
        StringKey::ErrNotFound => "Not found.",
        StringKey::ErrInvalidInput => "Invalid input.",
        StringKey::ErrAlreadyExists => "Item already exists.",
        StringKey::ErrAuthFailed => "Authentication failed.",
        StringKey::ErrIo => "I/O failure.",
        StringKey::ErrParse => "Document parsing failed.",
        StringKey::ErrEmbeddingFailed => "Embedding generation failed.",
        StringKey::ErrBackupCorrupt => "Backup is corrupt (checksum or decryption failed).",
        StringKey::ErrBackupVersion => "Backup schema version mismatch.",
        StringKey::ErrSubprocessTimeout => "Helper subprocess exceeded 60 seconds.",
        StringKey::ErrSubprocessStdoutOverflow => "Helper subprocess output exceeded 50 MB.",
        StringKey::ErrSubprocessArgvInvalid => {
            "Invalid helper subprocess arguments (shell metacharacters in path)."
        }
        // CLI help.
        StringKey::CliHelpTenantCreate => "Create a tenant and print the access token once.",
        StringKey::CliHelpIngest => "Ingest text or documents into memory.",
        StringKey::CliHelpSearch => "Search workspace memory.",
        StringKey::CliHelpRotateToken => "Rotate the tenant token (process restart required).",
        StringKey::CliHelpDelete => "Permanently delete memory.",
        StringKey::CliHelpContextFit => "Pack context from the most relevant chunks.",
        StringKey::CliHelpHealth => "Check service health.",
        StringKey::CliHelpCodeIndex => "Index a code project (Rust/Python).",
    }
}

/// Resolve a key for a locale (ES primary, EN fallback).
pub fn lookup(key: StringKey, locale: Locale) -> &'static str {
    match locale {
        Locale::Es => es(key),
        Locale::En => en(key),
    }
}
