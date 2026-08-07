//! Integration tests for the normalization boundary (T-031 acceptance).
//!
//! The subprocess path is exercised through the crate's fake-anydoc binary
//! (deterministic, offline); the fake mirrors the real `anydoc <input>`
//! CLI contract. A real-anydoc end-to-end test is gated with `#[ignore]`
//! (needs Node.js + network to resolve the pinned npm package).

use std::time::Duration;

use memento_domain::{DomainError, SourceKind};
use memento_parse::ParseService;
use memento_parse::anydoc::{AnydocCommand, AnydocConfig, DEFAULT_STDOUT_LIMIT, DEFAULT_TIMEOUT};
use memento_ports::parse::ParsePort;
use tempfile::TempDir;

fn fake_command(mode: &str) -> AnydocCommand {
    AnydocCommand {
        program: env!("CARGO_BIN_EXE_memento-parse-fake-anydoc").to_string(),
        args: Vec::new(),
        env: vec![("FAKE_ANYDOC_MODE".to_string(), mode.to_string())],
    }
}

fn service_with(mode: &str, staging: &TempDir) -> ParseService {
    ParseService::new(AnydocConfig {
        command: fake_command(mode),
        timeout: DEFAULT_TIMEOUT,
        stdout_limit: DEFAULT_STDOUT_LIMIT,
        staging_dir: staging.path().to_path_buf(),
    })
}

fn service_with_caps(
    mode: &str,
    timeout: Duration,
    stdout_limit: usize,
    staging: &TempDir,
) -> ParseService {
    ParseService::new(AnydocConfig {
        command: fake_command(mode),
        timeout,
        stdout_limit,
        staging_dir: staging.path().to_path_buf(),
    })
}

/// T-031 acceptance: a .docx blob routes to the subprocess and comes back
/// as Markdown (the fake wraps the staged input — proving the blob reached
/// the subprocess through the staging round trip).
#[tokio::test]
async fn docx_routes_to_anydoc_and_returns_markdown() {
    let staging = TempDir::new().expect("tempdir");
    let service = service_with("echo", &staging);

    let blob = b"PK\x03\x04 fake docx payload with Spanish text: memoria";
    let doc = service
        .parse(blob, SourceKind::Document("docx".into()))
        .await
        .expect("docx conversion");

    assert_eq!(doc.source_kind, SourceKind::Document("docx".into()));
    assert_eq!(doc.metadata["parser"], "anydoc");
    assert_eq!(doc.metadata["format"], "docx");
    assert_eq!(doc.metadata["source_bytes"], blob.len() as i64);
    assert!(
        doc.markdown.contains("fake docx payload"),
        "markdown must carry the staged blob content: {}",
        doc.markdown
    );
    // Staging is cleaned after a successful conversion (zero leftover writes).
    assert!(
        std::fs::read_dir(staging.path())
            .expect("read staging")
            .next()
            .is_none(),
        "staging dir must be empty after conversion"
    );
}

/// md must route to the fallback parser WITHOUT spawning the subprocess:
/// the fake is in bomb mode, so any subprocess attempt would overflow.
#[tokio::test]
async fn md_routes_to_fallback_without_subprocess() {
    let staging = TempDir::new().expect("tempdir");
    let service = service_with("bomb", &staging);

    let blob = b"# T\xc3\xadtulo\n\nContenido en espa\xc3\xb1ol.";
    let doc = service
        .parse(blob, SourceKind::Document("md".into()))
        .await
        .expect("md passthrough");

    assert_eq!(doc.markdown, "# Título\n\nContenido en español.");
    assert_eq!(doc.source_kind, SourceKind::Document("md".into()));
    assert_eq!(doc.metadata["parser"], "fallback:markdown");
}

/// Raw text routes to the fallback parser (REQ-MC-001 path).
#[tokio::test]
async fn text_kind_routes_to_fallback() {
    let staging = TempDir::new().expect("tempdir");
    let service = service_with("bomb", &staging);

    let blob = b"La memoria es la facultad de recordar.";
    let doc = service
        .parse(blob, SourceKind::Text)
        .await
        .expect("text passthrough");

    assert_eq!(doc.markdown, "La memoria es la facultad de recordar.");
    assert_eq!(doc.source_kind, SourceKind::Text);
    assert_eq!(doc.metadata["parser"], "fallback:text");
}

/// Unsupported formats are rejected with a structured validation error
/// before any subprocess attempt (REQ-MC-002 corrupt/unsupported scenario).
#[tokio::test]
async fn unsupported_format_rejected() {
    let staging = TempDir::new().expect("tempdir");
    let service = service_with("echo", &staging);

    let err = service
        .parse(b"blob", SourceKind::Document("xyz".into()))
        .await
        .expect_err("unsupported format");
    assert_eq!(err.code(), "INVALID_INPUT");
    assert!(err.to_string().contains("xyz"));

    // No staging writes for a rejected format.
    assert!(
        std::fs::read_dir(staging.path())
            .expect("read staging")
            .next()
            .is_none(),
        "staging dir must stay empty"
    );
}

/// T-031 acceptance: a corrupt document produces a stage-named structured
/// error and zero writes (REQ-MC-002, REQ-MC-007).
#[tokio::test]
async fn corrupt_doc_structured_error_zero_writes() {
    let staging = TempDir::new().expect("tempdir");
    let service = service_with("fail", &staging);

    let err = service
        .parse(
            b"PK\x03\x04 truncated garbage",
            SourceKind::Document("docx".into()),
        )
        .await
        .expect_err("corrupt doc");
    assert_eq!(err.code(), "PARSE");
    // The error names the failed stage (REQ-MC-007): parse/anydoc.
    assert!(
        err.to_string().contains("anydoc"),
        "stage-named error expected: {err}"
    );

    // Zero writes: staging cleaned even on failure.
    assert!(
        std::fs::read_dir(staging.path())
            .expect("read staging")
            .next()
            .is_none(),
        "staging dir must be empty after a failed conversion"
    );
}

/// Subprocess error codes propagate through the port unchanged: the RED
/// contract (T-030) holds at the service boundary too.
#[tokio::test]
async fn subprocess_errors_propagate_through_port() {
    let staging = TempDir::new().expect("tempdir");

    let hang = service_with_caps("hang", Duration::from_millis(300), 1024 * 1024, &staging);
    let err = hang
        .parse(b"blob", SourceKind::Document("docx".into()))
        .await
        .expect_err("hang");
    assert_eq!(err.code(), "SUBPROCESS_TIMEOUT");

    let bomb = service_with_caps("bomb", Duration::from_secs(30), 64 * 1024, &staging);
    let err = bomb
        .parse(b"blob", SourceKind::Document("docx".into()))
        .await
        .expect_err("bomb");
    assert_eq!(err.code(), "SUBPROCESS_STDOUT_OVERFLOW");
}

/// The argv gate also fires through the port (traversal in the extension).
#[tokio::test]
async fn malicious_extension_rejected_through_port() {
    let staging = TempDir::new().expect("tempdir");
    let service = service_with("echo", &staging);

    let err: DomainError = service
        .parse(b"blob", SourceKind::Document("../etc/passwd".into()))
        .await
        .expect_err("traversal");
    assert_eq!(err.code(), "SUBPROCESS_ARGV_INVALID");
}

/// Defaults guard: production caps are the documented ones.
#[test]
fn production_limits_match_design() {
    assert_eq!(DEFAULT_TIMEOUT, Duration::from_secs(60));
    assert_eq!(DEFAULT_STDOUT_LIMIT, 50 * 1024 * 1024);
}

/// Real anydoc end-to-end (.docx → Markdown), gated: needs Node.js + npm
/// reachable. Run explicitly with `cargo test -- --ignored` on a dev/CI
/// machine with network. Verified manually on the bootstrap host (2026-08-07).
#[tokio::test]
#[ignore = "requires Node.js + npm registry access"]
async fn docx_to_markdown_real_anydoc() {
    let staging = TempDir::new().expect("tempdir");
    let service =
        ParseService::auto(staging.path().to_path_buf()).expect("anydoc command resolved");

    // Minimal real .docx: a zip with the two mandatory parts.
    let docx = build_minimal_docx();
    let doc = service
        .parse(&docx, SourceKind::Document("docx".into()))
        .await
        .expect("real anydoc converts a minimal docx");
    assert!(
        doc.markdown.contains("Hola mundo"),
        "markdown must contain the document text: {}",
        doc.markdown
    );
}

/// Builds a minimal but valid .docx (ZIP) with one paragraph.
fn build_minimal_docx() -> Vec<u8> {
    let mut zip = zip_writer_for_tests::ZipWriter::default();
    zip.add(
        "[Content_Types].xml",
        br#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types">
  <Default Extension="rels" ContentType="application/vnd.openxmlformats-package.relationships+xml"/>
  <Default Extension="xml" ContentType="application/xml"/>
  <Override PartName="/word/document.xml" ContentType="application/vnd.openxmlformats-officedocument.wordprocessingml.document.main+xml"/>
</Types>"#,
    );
    zip.add(
        "_rels/.rels",
        br#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">
  <Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument" Target="word/document.xml"/>
</Relationships>"#,
    );
    zip.add(
        "word/document.xml",
        br#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<w:document xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main">
  <w:body>
    <w:p><w:r><w:t>Hola mundo</w:t></w:r></w:p>
  </w:body>
</w:document>"#,
    );
    zip.finish()
}

/// Minimal in-memory ZIP writer (store, no compression) — enough for anydoc
/// to parse a .docx; keeps the fixture dependency-free.
mod zip_writer_for_tests {
    use std::io::{Cursor, Write};

    #[derive(Default)]
    pub struct ZipWriter {
        entries: Vec<(String, Vec<u8>)>,
    }

    impl ZipWriter {
        pub fn add(&mut self, name: &str, data: &[u8]) {
            self.entries.push((name.to_string(), data.to_vec()));
        }

        pub fn finish(self) -> Vec<u8> {
            let mut out = Cursor::new(Vec::new());
            let mut central = Vec::new();
            let mut offset: u32 = 0;

            for (name, data) in &self.entries {
                let crc = crc32(data);
                let name_bytes = name.as_bytes();
                // Local file header: version, flags, method, time(2), date(2),
                // crc, csize, usize, name_len, extra_len, name.
                let mut lh = Cursor::new(Vec::new());
                lh.write_all(&[0x50, 0x4B, 0x03, 0x04]).unwrap(); // local file header
                lh.write_all(&20u16.to_le_bytes()).unwrap(); // version needed
                lh.write_all(&0u16.to_le_bytes()).unwrap(); // flags
                lh.write_all(&0u16.to_le_bytes()).unwrap(); // method: store
                lh.write_all(&0u16.to_le_bytes()).unwrap(); // mod time
                lh.write_all(&0u16.to_le_bytes()).unwrap(); // mod date
                lh.write_all(&crc.to_le_bytes()).unwrap();
                lh.write_all(&(data.len() as u32).to_le_bytes()).unwrap();
                lh.write_all(&(data.len() as u32).to_le_bytes()).unwrap();
                lh.write_all(&(name_bytes.len() as u16).to_le_bytes())
                    .unwrap();
                lh.write_all(&0u16.to_le_bytes()).unwrap(); // extra len
                lh.write_all(name_bytes).unwrap();
                let local_bytes = lh.into_inner();

                out.write_all(&local_bytes).unwrap();
                out.write_all(data).unwrap();

                let mut cd = Cursor::new(Vec::new());
                cd.write_all(&[0x50, 0x4B, 0x01, 0x02]).unwrap(); // central header
                cd.write_all(&20u16.to_le_bytes()).unwrap(); // version made by
                cd.write_all(&20u16.to_le_bytes()).unwrap(); // version needed
                cd.write_all(&0u16.to_le_bytes()).unwrap(); // flags
                cd.write_all(&0u16.to_le_bytes()).unwrap(); // method
                cd.write_all(&0u16.to_le_bytes()).unwrap(); // mod time
                cd.write_all(&0u16.to_le_bytes()).unwrap(); // mod date
                cd.write_all(&crc.to_le_bytes()).unwrap();
                cd.write_all(&(data.len() as u32).to_le_bytes()).unwrap(); // csize
                cd.write_all(&(data.len() as u32).to_le_bytes()).unwrap(); // usize
                cd.write_all(&(name_bytes.len() as u16).to_le_bytes())
                    .unwrap(); // name len
                cd.write_all(&0u16.to_le_bytes()).unwrap(); // extra len
                cd.write_all(&0u16.to_le_bytes()).unwrap(); // comment len
                cd.write_all(&0u16.to_le_bytes()).unwrap(); // disk number start
                cd.write_all(&0u16.to_le_bytes()).unwrap(); // internal attrs
                cd.write_all(&0u32.to_le_bytes()).unwrap(); // external attrs
                cd.write_all(&offset.to_le_bytes()).unwrap(); // local header offset
                cd.write_all(name_bytes).unwrap();
                central.extend_from_slice(&cd.into_inner());

                offset += local_bytes.len() as u32 + data.len() as u32;
            }

            let central_offset = offset;
            out.write_all(&central).unwrap();
            out.write_all(&[0x50, 0x4B, 0x05, 0x06]).unwrap(); // EOCD
            out.write_all(&0u16.to_le_bytes()).unwrap();
            out.write_all(&0u16.to_le_bytes()).unwrap();
            out.write_all(&(self.entries.len() as u16).to_le_bytes())
                .unwrap();
            out.write_all(&(self.entries.len() as u16).to_le_bytes())
                .unwrap();
            out.write_all(&(central.len() as u32).to_le_bytes())
                .unwrap();
            out.write_all(&central_offset.to_le_bytes()).unwrap();
            out.write_all(&0u16.to_le_bytes()).unwrap(); // comment len

            out.into_inner()
        }
    }

    fn crc32(data: &[u8]) -> u32 {
        let mut crc = 0xFFFF_FFFFu32;
        for &b in data {
            crc ^= b as u32;
            for _ in 0..8 {
                crc = if crc & 1 != 0 {
                    (crc >> 1) ^ 0xEDB8_8320
                } else {
                    crc >> 1
                };
            }
        }
        !crc
    }
}
