//! Shared helpers for the memento-e2e criterion benches (T-103).
//!
//! Each `benches/*.rs` file is its own crate; this module is pulled in via
//! `#[path = "common/mod.rs"]` so every bench shares the same corpus
//! generator and app-builder without a third helper crate.
//!
//! Honest-bench rules (REQ-MR-007 / REQ-CK-002 "deviations MUST be reported
//! with benchmark evidence"): every gate prints one `MEMBENCH <key> <json>`
//! line that `scripts/bench.sh` greps and checks against the spec budgets.
//! Corpus sizes are configurable through `MEMENTO_BENCH_CHUNKS` /
//! `MEMENTO_BENCH_LOC` so the reference corpus (100k chunks, 100k LOC) can
//! be scaled down for fast smoke runs.
#![allow(dead_code)] // each bench crate compiles only the helpers it uses

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use memento_application::{AppService, SystemClock};
use memento_domain::TenantContext;
use memento_parse::ParseService;
use memento_parse::anydoc::{AnydocCommand, AnydocConfig};
use memento_ports::ParsePort;
use serde_json::Value;

/// Spanish topic sentences. The repeated topic words (memoria, río,
/// documento, historia, archivo, conocimiento, biblioteca) give the BM25
/// ranking real structure to work with — realistic for the search bench.
const SENTENCES: [&str; 24] = [
    "La memoria es un río subterráneo que fluye entre documentos antiguos y nuevos.",
    "Cada archivo de la historia guarda conocimiento valioso para el futuro.",
    "El río de la memoria arrastra recuerdos, fechas y nombres de personas.",
    "Los documentos de la biblioteca contienen la historia completa de la región.",
    "El conocimiento fluye como el agua cuando los archivos se organizan bien.",
    "Memoria y documento se unen para preservar la historia de cada generación.",
    "La biblioteca del pueblo conserva archivos que narran la memoria colectiva.",
    "Un buen archivo ordena el conocimiento y evita que la historia se pierda.",
    "El documento más antiguo del río documenta la fundación de la ciudad.",
    "La historia oral se transforma en documento cuando alguien la escribe.",
    "Los archivos de la memoria crecen con cada nuevo conocimiento acumulado.",
    "El agua del río recuerda los caminos que los documentos describen.",
    "Conservar la memoria exige organizar los documentos con cuidado.",
    "La historia de la región vive en los archivos de la biblioteca pública.",
    "Cada documento nuevo enriquece el conocimiento de la comunidad entera.",
    "La memoria colectiva se apoya en archivos abiertos y bien ordenados.",
    "El río de la historia arrastra conocimiento de todas las generaciones.",
    "Los documentos antiguos revelan la memoria oculta de la ciudad.",
    "La biblioteca organiza el conocimiento y lo pone al servicio del pueblo.",
    "Archivar es proteger la memoria para las generaciones futuras.",
    "El conocimiento del río se guarda en documentos de cada época.",
    "La historia se escribe con los archivos que la memoria conserva.",
    "Documentos, archivos y bibliotecas sostienen la memoria de todos.",
    "El pueblo celebra su historia gracias a los archivos bien conservados.",
];

/// The chunk count target for the search-bench corpus. The reference corpus
/// is 100k chunks (REQ-MR-007); `MEMENTO_BENCH_CHUNKS` scales it down for
/// fast smoke runs.
pub fn bench_chunks() -> usize {
    std::env::var("MEMENTO_BENCH_CHUNKS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(100_000)
}

/// Emit one gate line for `scripts/bench.sh`.
pub fn report(key: &str, value: Value) {
    println!("MEMBENCH {key} {value}");
}

/// A parse boundary that is never invoked (benches ingest text only).
pub fn never_parse() -> Arc<dyn ParsePort> {
    Arc::new(ParseService::new(AnydocConfig {
        command: AnydocCommand {
            program: "never-invoked".into(),
            args: vec![],
            env: vec![],
        },
        timeout: Duration::from_secs(1),
        stdout_limit: 1024,
        staging_dir: std::env::temp_dir(),
    }))
}

/// Deterministic Spanish doc sized to split into roughly `target_chunks`
/// chunks of 256-300 tokens (T-032 bounds). `salt` varies the content so
/// repeated ingests never hit the content-hash dedup (REQ-MC-005) — the
/// bench must measure real writes.
///
/// Sentence budget: ~24 sentences per chunk (~11 tokens each ≈ 264 tokens).
/// Overlap (~10-15%) raises the real count slightly, so the generator
/// over-allocates by 12% — the exact count is reported by the caller.
pub fn doc_text(target_chunks: usize, salt: usize) -> String {
    let sentences = target_chunks * 26 + salt % 7;
    let mut text = String::with_capacity(sentences * 90);
    for i in 0..sentences {
        let s = SENTENCES[(i + salt) % SENTENCES.len()];
        text.push_str(s);
        text.push('\n');
    }
    text
}

/// `docs` documents, each sized to `total_chunks / docs` chunks.
pub fn corpus_docs(total_chunks: usize, docs: usize) -> Vec<String> {
    let per_doc = (total_chunks / docs).max(1);
    (0..docs).map(|i| doc_text(per_doc, i * 7 + 1)).collect()
}

/// Query pool: short Spanish phrases over the recurring topic words. Every
/// query matches many chunks, so the measured work is a realistic BM25
/// ranking pass over the corpus.
pub fn queries() -> Vec<String> {
    [
        "memoria río",
        "documento historia",
        "archivo conocimiento",
        "biblioteca pueblo",
        "memoria documento",
        "río historia",
        "archivo memoria",
        "conocimiento río",
        "historia generación",
        "documento antiguo",
        "biblioteca archivo",
        "memoria colectiva",
        "agua río",
        "ciudad historia",
        "archivo abierto",
        "memoria oculta",
        "documento nuevo",
        "pueblo historia",
        "conocimiento comunidad",
        "río subterráneo",
        "archivo fechas",
        "memoria generaciones",
        "historia región",
        "biblioteca pública",
        "documento ciudad",
        "memoria conocimiento",
        "río documento",
        "archivo historia",
        "memoria pueblo",
        "conocimiento documento",
        "historia archivo",
        "memoria biblioteca",
    ]
    .into_iter()
    .map(String::from)
    .collect()
}

/// Open an application service over `root` with a never-invoked parse
/// boundary, the deterministic stub embedder (D2; no ONNX download) and the
/// production clock. FTS retrieval (the default, REQ-MR-001) needs no
/// vectors, so the stub keeps the pipeline identical to production minus
/// the model download.
pub fn app(root: &Path, ctx: &TenantContext) -> AppService {
    let rt = tokio::runtime::Runtime::new().expect("bench tokio runtime");
    rt.block_on(AppService::open(
        ctx,
        root,
        never_parse(),
        Some(Arc::new(memento_testkit::StubEmbedPort::default())),
        Arc::new(SystemClock),
    ))
    .expect("bench app opens")
}

/// p-quantile over a sorted slice (linear interpolation, inclusive p).
pub fn percentile(sorted: &[f64], p: f64) -> f64 {
    assert!(!sorted.is_empty(), "percentile needs samples");
    let rank = (p * (sorted.len() - 1) as f64).round() as usize;
    sorted[rank]
}

/// Time `f` and return (elapsed, output).
pub fn timed<T>(f: impl FnOnce() -> T) -> (Duration, T) {
    let start = std::time::Instant::now();
    let out = f();
    (start.elapsed(), out)
}
