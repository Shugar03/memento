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
    ErrRerankFailed,
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
    CliHelpRoot,
    CliHelpTenant,
    CliHelpTenantDelete,
    CliHelpTenantExport,
    CliHelpTenantRetention,
    CliHelpTenantBackup,
    CliHelpTenantRestore,
    CliHelpTenantSweep,
    CliHelpIngestDocument,
    CliHelpIngestBulk,
    CliHelpGetChunk,
    CliHelpFeedback,
    CliHelpStats,
    CliHelpCodeStatus,
    CliHelpCodeDebug,
    CliHelpJson,
    CliHelpNoEmbeddings,
    CliHelpRootArg,
    CliHelpLocaleArg,
    CliHelpQueryArg,
    CliHelpTextArg,
    CliHelpTopKArg,
    CliHelpWorkspaceArg,
    CliHelpRrfArg,
    CliHelpRrfKArg,
    CliHelpRerankArg,
    CliHelpNameArg,
    CliHelpDaysArg,
    CliHelpChunkArg,
    CliHelpDocArg,
    CliHelpUsefulArg,
    CliHelpNotUsefulArg,
    CliHelpReasonArg,
    CliHelpBudgetArg,
    CliHelpFileArg,
    CliHelpDirArg,
    CliHelpSourceArg,
    CliHelpProjectArg,
    CliHelpBackupDirArg,
    CliHelpPathArg,
    CliMsgTokenCreated,
    CliMsgTokenRotated,
    CliPromptConfirmDelete,
    // Observability commands (REQ-OBS-007, design D7).
    CliHelpObservability,
    CliHelpObservabilityMetrics,
}

impl StringKey {
    /// Every key, so tests can prove ES/EN parity.
    pub const ALL: [StringKey; 91] = [
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
        StringKey::ErrRerankFailed,
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
        StringKey::CliHelpRoot,
        StringKey::CliHelpTenant,
        StringKey::CliHelpTenantDelete,
        StringKey::CliHelpTenantExport,
        StringKey::CliHelpTenantRetention,
        StringKey::CliHelpTenantBackup,
        StringKey::CliHelpTenantRestore,
        StringKey::CliHelpTenantSweep,
        StringKey::CliHelpIngestDocument,
        StringKey::CliHelpIngestBulk,
        StringKey::CliHelpGetChunk,
        StringKey::CliHelpFeedback,
        StringKey::CliHelpStats,
        StringKey::CliHelpCodeStatus,
        StringKey::CliHelpCodeDebug,
        StringKey::CliHelpJson,
        StringKey::CliHelpNoEmbeddings,
        StringKey::CliHelpRootArg,
        StringKey::CliHelpLocaleArg,
        StringKey::CliHelpQueryArg,
        StringKey::CliHelpTextArg,
        StringKey::CliHelpTopKArg,
        StringKey::CliHelpWorkspaceArg,
        StringKey::CliHelpRrfArg,
        StringKey::CliHelpRrfKArg,
        StringKey::CliHelpRerankArg,
        StringKey::CliHelpNameArg,
        StringKey::CliHelpDaysArg,
        StringKey::CliHelpChunkArg,
        StringKey::CliHelpDocArg,
        StringKey::CliHelpUsefulArg,
        StringKey::CliHelpNotUsefulArg,
        StringKey::CliHelpReasonArg,
        StringKey::CliHelpBudgetArg,
        StringKey::CliHelpFileArg,
        StringKey::CliHelpDirArg,
        StringKey::CliHelpSourceArg,
        StringKey::CliHelpProjectArg,
        StringKey::CliHelpBackupDirArg,
        StringKey::CliHelpPathArg,
        StringKey::CliMsgTokenCreated,
        StringKey::CliMsgTokenRotated,
        StringKey::CliPromptConfirmDelete,
        StringKey::CliHelpObservability,
        StringKey::CliHelpObservabilityMetrics,
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
        StringKey::ErrRerankFailed => "Falló el reordenamiento con el cross-encoder.",
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
        StringKey::CliHelpRoot => {
            "Memento: memoria de agentes con tenancy, búsqueda y código (ES-first)."
        }
        StringKey::CliHelpTenant => {
            "Administración del tenant: credenciales, retención, respaldos, exportación y borrado."
        }
        StringKey::CliHelpTenantDelete => {
            "Borra el tenant: purga de datos, destrucción de claves y credenciales (requiere confirmación)."
        }
        StringKey::CliHelpTenantExport => {
            "Exporta todos los datos del tenant en formato abierto (JSONL + tar.gz)."
        }
        StringKey::CliHelpTenantRetention => {
            "Muestra o configura el horizonte de retención (días; 0 desactiva)."
        }
        StringKey::CliHelpTenantBackup => "Crea un respaldo cifrado del tenant.",
        StringKey::CliHelpTenantRestore => "Restaura un respaldo (requiere almacén en reposo).",
        StringKey::CliHelpTenantSweep => "Ejecuta la limpieza de retención inmediatamente.",
        StringKey::CliHelpIngestDocument => "Ingresa un archivo de documento (14 formatos).",
        StringKey::CliHelpIngestBulk => "Ingesta masiva de un directorio con informe por archivo.",
        StringKey::CliHelpGetChunk => "Obtiene un fragmento de memoria por id.",
        StringKey::CliHelpFeedback => "Registra retroalimentación de utilidad sobre un fragmento.",
        StringKey::CliHelpStats => "Muestra estadísticas del almacén por workspace.",
        StringKey::CliHelpCodeStatus => "Estado del índice de código (capas L1-L4).",
        StringKey::CliHelpCodeDebug => {
            "Diagnóstico del grafo del índice (nodos, aristas, integridad)."
        }
        StringKey::CliHelpJson => "Salida en JSON (REQ-CL-003).",
        StringKey::CliHelpNoEmbeddings => "Sin embeddings (REQ-MC-004).",
        StringKey::CliHelpRootArg => "Raíz de almacenamiento (por defecto ~/.memento).",
        StringKey::CliHelpLocaleArg => "Idioma de la interfaz: es | en.",
        StringKey::CliHelpQueryArg => "Consulta de búsqueda.",
        StringKey::CliHelpTextArg => "Texto a ingestar.",
        StringKey::CliHelpTopKArg => "Cantidad de resultados (máx. 100).",
        StringKey::CliHelpWorkspaceArg => "Workspace (por defecto el del tenant).",
        StringKey::CliHelpRrfArg => "Búsqueda híbrida con RRF.",
        StringKey::CliHelpRrfKArg => "Constante k de fusión RRF (híbrido; por defecto 60).",
        StringKey::CliHelpRerankArg => {
            "Reordenar los primeros resultados con el cross-encoder (requiere MEMENTO_RERANK=1; solo híbrido)."
        }
        StringKey::CliHelpNameArg => "Nombre del tenant.",
        StringKey::CliHelpDaysArg => "Días de retención (0 desactiva).",
        StringKey::CliHelpChunkArg => "Id del fragmento.",
        StringKey::CliHelpDocArg => "Id del documento.",
        StringKey::CliHelpUsefulArg => "Marca el fragmento como relevante.",
        StringKey::CliHelpNotUsefulArg => "Marca el fragmento como irrelevante.",
        StringKey::CliHelpReasonArg => "Motivo (opcional).",
        StringKey::CliHelpBudgetArg => "Presupuesto de tokens.",
        StringKey::CliHelpFileArg => "Archivo a ingestar.",
        StringKey::CliHelpDirArg => "Directorio a ingestar en masa.",
        StringKey::CliHelpSourceArg => "Fuente: text | markdown | document:<ext>.",
        StringKey::CliHelpProjectArg => "Id de proyecto (por defecto: derivado de la ruta).",
        StringKey::CliHelpBackupDirArg => "Directorio del respaldo (backups/<tid>/<ts>).",
        StringKey::CliHelpPathArg => "Ruta del proyecto de código.",
        StringKey::CliMsgTokenCreated => {
            "Guarde este token: se muestra una sola vez y solo su hash queda almacenado."
        }
        StringKey::CliMsgTokenRotated => {
            "Token rotado: el token anterior dejó de ser válido; reinicie el proceso."
        }
        StringKey::CliPromptConfirmDelete => {
            "Escriba 'yes' para confirmar el borrado permanente del tenant {tid}:"
        }
        // Observability commands (REQ-OBS-007).
        StringKey::CliHelpObservability => {
            "Observabilidad: métricas locales del proceso (sin HTTP)."
        }
        StringKey::CliHelpObservabilityMetrics => {
            "Vuelca el registro de métricas como texto Prometheus (stdout o MEMENTO_METRICS_FILE)."
        }
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
        StringKey::ErrRerankFailed => "Cross-encoder rerank failed.",
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
        StringKey::CliHelpRoot => {
            "Memento: agent memory with tenancy, search, and code knowledge (ES-first)."
        }
        StringKey::CliHelpTenant => {
            "Tenant administration: credentials, retention, backups, export, and erasure."
        }
        StringKey::CliHelpTenantDelete => {
            "Delete the tenant: data purge, key destruction, and credential removal (confirmation required)."
        }
        StringKey::CliHelpTenantExport => {
            "Export all tenant data in an open format (JSONL + tar.gz)."
        }
        StringKey::CliHelpTenantRetention => {
            "Show or set the retention horizon (days; 0 disables)."
        }
        StringKey::CliHelpTenantBackup => "Create an encrypted tenant backup.",
        StringKey::CliHelpTenantRestore => "Restore a backup (requires a quiesced store).",
        StringKey::CliHelpTenantSweep => "Run the retention sweep now.",
        StringKey::CliHelpIngestDocument => "Ingest a document file (14 formats).",
        StringKey::CliHelpIngestBulk => "Bulk-ingest a directory with a per-file report.",
        StringKey::CliHelpGetChunk => "Fetch a memory chunk by id.",
        StringKey::CliHelpFeedback => "Record usefulness feedback on a chunk.",
        StringKey::CliHelpStats => "Show store statistics per workspace.",
        StringKey::CliHelpCodeStatus => "Code index status (layers L1-L4).",
        StringKey::CliHelpCodeDebug => "Index graph diagnostics (nodes, edges, integrity).",
        StringKey::CliHelpJson => "JSON output (REQ-CL-003).",
        StringKey::CliHelpNoEmbeddings => "No embeddings (REQ-MC-004).",
        StringKey::CliHelpRootArg => "Storage root (default ~/.memento).",
        StringKey::CliHelpLocaleArg => "Interface locale: es | en.",
        StringKey::CliHelpQueryArg => "Search query.",
        StringKey::CliHelpTextArg => "Text to ingest.",
        StringKey::CliHelpTopKArg => "Result count (max 100).",
        StringKey::CliHelpWorkspaceArg => "Workspace (defaults to the tenant's).",
        StringKey::CliHelpRrfArg => "Hybrid search with RRF.",
        StringKey::CliHelpRrfKArg => "RRF fusion constant k (hybrid; default 60).",
        StringKey::CliHelpRerankArg => {
            "Rerank the top candidates with the cross-encoder (requires MEMENTO_RERANK=1; hybrid only)."
        }
        StringKey::CliHelpNameArg => "Tenant name.",
        StringKey::CliHelpDaysArg => "Retention days (0 disables).",
        StringKey::CliHelpChunkArg => "Chunk id.",
        StringKey::CliHelpDocArg => "Document id.",
        StringKey::CliHelpUsefulArg => "Mark the chunk as useful.",
        StringKey::CliHelpNotUsefulArg => "Mark the chunk as not useful.",
        StringKey::CliHelpReasonArg => "Reason (optional).",
        StringKey::CliHelpBudgetArg => "Token budget.",
        StringKey::CliHelpFileArg => "File to ingest.",
        StringKey::CliHelpDirArg => "Directory to bulk-ingest.",
        StringKey::CliHelpSourceArg => "Source: text | markdown | document:<ext>.",
        StringKey::CliHelpProjectArg => "Project id (default: derived from the path).",
        StringKey::CliHelpBackupDirArg => "Backup directory (backups/<tid>/<ts>).",
        StringKey::CliHelpPathArg => "Code project path.",
        StringKey::CliMsgTokenCreated => {
            "Save this token: it is shown only once and only its hash is stored."
        }
        StringKey::CliMsgTokenRotated => {
            "Token rotated: the previous token is invalid; restart the process."
        }
        StringKey::CliPromptConfirmDelete => {
            "Type 'yes' to confirm permanent deletion of tenant {tid}:"
        }
        // Observability commands (REQ-OBS-007).
        StringKey::CliHelpObservability => {
            "Observability: process-local metrics (no HTTP)."
        }
        StringKey::CliHelpObservabilityMetrics => {
            "Dump the metrics registry as Prometheus text (stdout or MEMENTO_METRICS_FILE)."
        }
    }
}

/// Resolve a key for a locale (ES primary, EN fallback).
pub fn lookup(key: StringKey, locale: Locale) -> &'static str {
    match locale {
        Locale::Es => es(key),
        Locale::En => en(key),
    }
}
