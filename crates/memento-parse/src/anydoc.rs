//! anydoc subprocess adapter (T-031, threat matrix rows 1-2).
//!
//! Converts document blobs to Markdown through the `@firecrawl/anydoc`
//! CLI (pinned 0.1.7, see `docs/dependencies.md`), spawned as a subprocess
//! with a strict isolation contract:
//!
//! * **Positional argv, no shell.** The child command line is built with
//!   `Command` (never a shell string). The only user-controlled fragment in
//!   argv is the *file extension* from `SourceKind::Document(ext)`; it is
//!   allowlist-validated (`[a-z0-9]`, ≤ 8 chars) before anything is staged.
//!   On Windows this also neutralizes `cmd.exe` expansion of `%VAR%`/`!VAR!`
//!   when `.cmd` shims (npx) are spawned — see [`detect_anydoc_command`]
//!   for how the npm entry is run through `node.exe` instead.
//! * **Basename-only paths.** The child runs with the staging dir as its
//!   working directory and receives only the file *basename* — server-owned
//!   uuid + validated extension. No user path ever reaches argv.
//! * **50 MiB stdout cap.** Stdout is streamed and aborted (with kill) the
//!   moment it exceeds the configured limit (default 50 MiB).
//! * **60 s timeout, kill-on-timeout.** A child that does not exit within
//!   the timeout is killed and surfaced as `SubprocessTimeout`.
//!
//! Errors map to the stable subprocess taxonomy (memento-domain D7):
//! `SubprocessArgvInvalid` (32), `SubprocessStdoutOverflow` (31),
//! `SubprocessTimeout` (30). A non-zero exit is a stage-named `Parse`
//! failure (REQ-MC-007). Staging is cleaned on every path — a failed
//! conversion leaves zero writes.

use std::path::{Path, PathBuf};
use std::time::Duration;

use memento_domain::DomainError;
use tokio::io::AsyncReadExt;
use tokio::process::Command;

/// Default subprocess timeout (design: hang > 60s → kill).
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(60);
/// Default stdout capture cap (design: output bomb > 50MB → abort).
pub const DEFAULT_STDOUT_LIMIT: usize = 50 * 1024 * 1024;
/// Hard cap on stderr captured for error reporting (bounded diagnostics).
pub const STDERR_LIMIT: usize = 64 * 1024;
/// Extension allowlist rule: lowercase alphanumeric only, ≤ 8 chars.
/// This is the argv-injection/traversal gate — the only user-controlled
/// fragment that reaches the command line.
pub const MAX_EXT_LEN: usize = 8;

/// Supported document extensions routed to the anydoc subprocess (from the
/// upstream README — Word/PowerPoint/Excel/OpenDocument/RTF/EPUB/CSV/PDF).
/// md/txt/markdown/text are handled by the fallback parser instead.
pub const ANYDOC_EXTENSIONS: &[&str] = &[
    "doc", "docx", "docm", "odt", "rtf", "epub", "pdf", "ppt", "pps", "pot", "pptx", "pptm",
    "ppsx", "ppsm", "odp", "xls", "xlsx", "xlsm", "xlsb", "ods", "csv",
];

/// Result of a successful conversion.
#[derive(Debug, Clone, PartialEq)]
pub struct Converted {
    pub markdown: String,
    /// Normalized extension that was converted.
    pub format: String,
    /// Size of the input blob in bytes.
    pub input_bytes: usize,
}

/// How to invoke the anydoc converter. `program` + `args` are the argv
/// prefix (the staged input basename is appended); `env` allows tests to
/// steer a fake binary without touching argv.
#[derive(Debug, Clone)]
pub struct AnydocCommand {
    pub program: String,
    pub args: Vec<String>,
    pub env: Vec<(String, String)>,
}

/// Tunables for the subprocess boundary. Tests inject a fake binary and
/// small caps; production uses the defaults (60 s / 50 MiB) with the
/// staging dir from the storage layout (D8 `db/tmp`).
#[derive(Debug, Clone)]
pub struct AnydocConfig {
    pub command: AnydocCommand,
    pub timeout: Duration,
    pub stdout_limit: usize,
    pub staging_dir: PathBuf,
}

/// The anydoc subprocess boundary.
#[derive(Debug, Clone)]
pub struct AnydocClient {
    config: AnydocConfig,
}

/// Windows: never pop a console window for the child.
#[cfg(windows)]
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

impl AnydocClient {
    pub fn new(config: AnydocConfig) -> Self {
        Self { config }
    }

    /// Normalize a document blob to Markdown.
    ///
    /// # Errors
    ///
    /// * `SubprocessArgvInvalid` — `ext` fails the allowlist (traversal,
    ///   metacharacters, too long). Nothing is staged or executed.
    /// * `SubprocessStdoutOverflow` — child stdout exceeded the cap; child
    ///   killed.
    /// * `SubprocessTimeout` — child did not exit in time; child killed.
    /// * `Parse` — child exited non-zero (stage-named).
    /// * `Io` — staging write / spawn / pipe failure.
    pub async fn convert(&self, blob: &[u8], ext: &str) -> Result<Converted, DomainError> {
        let ext = validate_ext(ext)?;

        // Stage the blob under a server-generated name: uuid + validated ext.
        let staging = staging_path(&self.config.staging_dir, &ext);
        if let Some(parent) = staging.parent() {
            std::fs::create_dir_all(parent).map_err(|e| DomainError::Io { source: e })?;
        }
        tokio::fs::write(&staging, blob)
            .await
            .map_err(|e| DomainError::Io { source: e })?;

        let result = self.convert_staged(&staging, &ext).await;

        // Best-effort cleanup on every path: a failed conversion leaves
        // zero writes (REQ-MC-002 / REQ-MC-007).
        let _ = tokio::fs::remove_file(&staging).await;

        result.map(|markdown| Converted {
            markdown,
            format: ext,
            input_bytes: blob.len(),
        })
    }

    async fn convert_staged(&self, input: &Path, ext: &str) -> Result<String, DomainError> {
        // Basename-only argv (design): the child runs in the staging dir.
        let basename = input
            .file_name()
            .and_then(|n| n.to_str())
            .expect("staged file name is valid UTF-8 by construction");

        let mut cmd = Command::new(&self.config.command.program);
        cmd.args(&self.config.command.args)
            .arg(basename)
            .current_dir(&self.config.staging_dir)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true);
        for (k, v) in &self.config.command.env {
            cmd.env(k, v);
        }
        #[cfg(windows)]
        cmd.creation_flags(CREATE_NO_WINDOW);

        let mut child = cmd.spawn().map_err(|e| DomainError::Io { source: e })?;
        let mut stdout = child.stdout.take().ok_or_else(|| DomainError::Internal {
            message: "anydoc stdout pipe unavailable".into(),
        })?;
        let mut stderr_pipe = child.stderr.take().ok_or_else(|| DomainError::Internal {
            message: "anydoc stderr pipe unavailable".into(),
        })?;

        // Stream stdout with a hard cap and kill-on-timeout.
        let mut markdown: Vec<u8> = Vec::new();
        let mut buf = [0u8; 16 * 1024];
        let timeout = tokio::time::sleep(self.config.timeout);
        tokio::pin!(timeout);

        let (total, overflowed) = loop {
            tokio::select! {
                _ = &mut timeout => {
                    child.kill().await.map_err(|e| DomainError::Io { source: e })?;
                    let _ = child.wait().await; // reap
                    return Err(DomainError::SubprocessTimeout {
                        command: self.config.command.program.clone(),
                    });
                }
                read = stdout.read(&mut buf) => match read {
                    Ok(0) => break (markdown.len(), false),
                    Ok(n) => {
                        markdown.extend_from_slice(&buf[..n]);
                        if markdown.len() > self.config.stdout_limit {
                            break (markdown.len(), true);
                        }
                    }
                    Err(e) => {
                        child.kill().await.ok();
                        let _ = child.wait().await;
                        return Err(DomainError::Io { source: e });
                    }
                },
            }
        };

        if overflowed {
            child
                .kill()
                .await
                .map_err(|e| DomainError::Io { source: e })?;
            let _ = child.wait().await; // reap
            return Err(DomainError::SubprocessStdoutOverflow {
                bytes: total as u64,
            });
        }

        // stdout closed: the child must still exit (bounded by the timeout).
        match tokio::time::timeout(self.config.timeout, child.wait())
            .await
            .map_err(|_| DomainError::SubprocessTimeout {
                command: self.config.command.program.clone(),
            })? {
            Err(e) => return Err(DomainError::Io { source: e }),
            Ok(status) => {
                if !status.success() {
                    let mut stderr = Vec::new();
                    let mut sbuf = [0u8; 4096];
                    loop {
                        match stderr_pipe.read(&mut sbuf).await {
                            Ok(0) | Err(_) => break,
                            Ok(n) => {
                                stderr.extend_from_slice(&sbuf[..n]);
                                if stderr.len() > STDERR_LIMIT {
                                    break;
                                }
                            }
                        }
                    }
                    let detail = String::from_utf8_lossy(&stderr);
                    let detail = if detail.trim().is_empty() {
                        format!("exit code {}", status.code().unwrap_or(-1))
                    } else {
                        format!(
                            "exit code {}: {}",
                            status.code().unwrap_or(-1),
                            detail.trim()
                        )
                    };
                    return Err(DomainError::Parse {
                        // Stage-named error (REQ-MC-007): parse/anydoc.
                        message: format!("anydoc normalization failed ({ext}): {detail}"),
                    });
                }
            }
        }

        String::from_utf8(markdown).map_err(|_| DomainError::Parse {
            message: "anydoc output is not valid UTF-8".into(),
        })
    }
}

/// Allowlist-validate the extension fragment that reaches argv.
///
/// The extension is the only user-controlled string in the child command
/// line, so it is gated hard: lowercase `[a-z0-9]`, length 1..=8, a single
/// leading dot tolerated. Everything else (path separators, `..`, shell
/// metacharacters, `%`, `!`, quotes, spaces, unicode) is rejected with
/// `SubprocessArgvInvalid` — before any staging write or exec.
pub fn validate_ext(ext: &str) -> Result<String, DomainError> {
    let ext = ext.strip_prefix('.').unwrap_or(ext).to_ascii_lowercase();
    if ext.is_empty() || ext.len() > MAX_EXT_LEN {
        return Err(DomainError::SubprocessArgvInvalid {
            detail: format!("extension '{ext}' rejected: invalid length"),
        });
    }
    if !ext
        .bytes()
        .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit())
    {
        return Err(DomainError::SubprocessArgvInvalid {
            detail: format!("extension '{ext}' rejected: non [a-z0-9] character"),
        });
    }
    Ok(ext)
}

/// Server-generated staging file name: uuid + validated ext (never user
/// input — the ext has already passed `validate_ext`).
fn staging_path(staging_dir: &Path, ext: &str) -> PathBuf {
    let name = format!("{}.{}", uuid::Uuid::now_v7().simple(), ext);
    staging_dir.join(name)
}

/// Resolve the anydoc converter: a globally installed `anydoc` binary on
/// PATH, else the pinned npm package run through `node` (shell-free).
///
/// On Windows, `.cmd` shims (npx.cmd) cannot be spawned directly by
/// `CreateProcess` (error 193), and wrapping them in `cmd /C` would
/// reintroduce shell parsing — so the resolver locates the real npx JS
/// entry (`<node-dir>/node_modules/npm/bin/npx-cli.js`) and runs it with
/// `node.exe`: argv stays positional, zero shell, exactly like Unix.
///
/// The pinned package spec `@firecrawl/anydoc@0.1.7` is a static literal;
/// the only dynamic fragment in argv is the allowlist-validated staging
/// basename (see module docs).
pub fn detect_anydoc_command() -> Result<AnydocCommand, DomainError> {
    if let Some(program) = find_on_path("anydoc") {
        return Ok(AnydocCommand {
            program,
            args: Vec::new(),
            env: Vec::new(),
        });
    }
    if let Some((node, npx_cli)) =
        find_on_path("node").and_then(|node| npx_cli_script(&node).map(|npx_cli| (node, npx_cli)))
    {
        return Ok(AnydocCommand {
            program: node,
            args: vec![npx_cli, "--yes".into(), "@firecrawl/anydoc@0.1.7".into()],
            env: Vec::new(),
        });
    }
    Err(DomainError::Parse {
        message: "anydoc not found: install Node.js with @firecrawl/anydoc (npm) or put the anydoc binary on PATH"
            .into(),
    })
}

/// The npx JS entry shipped inside the npm install that owns `node`.
fn npx_cli_script(node_exe: &str) -> Option<String> {
    let node_dir = Path::new(node_exe).parent()?;
    let candidate = node_dir
        .join("node_modules")
        .join("npm")
        .join("bin")
        .join("npx-cli.js");
    candidate
        .is_file()
        .then(|| candidate.to_str().map(str::to_string))
        .flatten()
}

/// PATH lookup that respects Windows executable extensions (.exe/.cmd/.bat).
fn find_on_path(program: &str) -> Option<String> {
    let path_var = std::env::var_os("PATH")?;
    let exts: &[&str] = if cfg!(windows) {
        &["", ".exe", ".cmd", ".bat"]
    } else {
        &[""]
    };
    for dir in std::env::split_paths(&path_var) {
        if dir.as_os_str().is_empty() {
            continue;
        }
        for ext in exts {
            let candidate = dir.join(format!("{program}{ext}"));
            if candidate.is_file() {
                return candidate.to_str().map(str::to_string);
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validate_ext_normalizes_and_accepts_known() {
        assert_eq!(validate_ext("docx").unwrap(), "docx");
        assert_eq!(validate_ext("DOCX").unwrap(), "docx"); // case-folded
        assert_eq!(validate_ext(".pdf").unwrap(), "pdf"); // leading dot tolerated
        assert_eq!(validate_ext("xlsb").unwrap(), "xlsb");
        assert_eq!(validate_ext("csv").unwrap(), "csv");
    }

    #[test]
    fn validate_ext_rejects_traversal() {
        for evil in [
            "../etc/passwd",
            "..\\..\\win",
            "a/../../b",
            "/etc/shadow",
            "C:\\evil",
            "..",
            ".",
        ] {
            let err = validate_ext(evil).unwrap_err();
            assert_eq!(err.code(), "SUBPROCESS_ARGV_INVALID", "ext {evil:?}");
        }
    }

    #[test]
    fn validate_ext_rejects_metacharacters_and_weirdness() {
        for evil in [
            "docx;rm", "docx&x", "docx|x", "docx>o", "docx<i", "docx`x`", "docx$x", "docx\\x",
            "docx%x", "docx!x", "docx\"x", "docx'x", "doc x", "docx ", "docx€",
            "x", // too short is fine, but:
        ] {
            if evil == "x" {
                continue; // single alnum char is valid
            }
            let err = validate_ext(evil).unwrap_err();
            assert_eq!(err.code(), "SUBPROCESS_ARGV_INVALID", "ext {evil:?}");
        }
        // Length cap: > 8 chars.
        let err = validate_ext("abcdefgh9").unwrap_err();
        assert_eq!(err.code(), "SUBPROCESS_ARGV_INVALID");
    }

    #[test]
    fn validate_ext_rejects_empty() {
        let err = validate_ext("").unwrap_err();
        assert_eq!(err.code(), "SUBPROCESS_ARGV_INVALID");
    }

    #[test]
    fn staging_name_is_server_generated() {
        let dir = Path::new("/tmp/staging");
        let p = staging_path(dir, "docx");
        let name = p.file_name().unwrap().to_str().unwrap();
        // uuid (32 hex) + "." + validated ext — nothing user-controlled.
        let (id, ext) = name.split_once('.').expect("dot separator");
        assert_eq!(ext, "docx");
        assert_eq!(id.len(), 32, "uuid part: {id}");
        assert!(id.chars().all(|c| c.is_ascii_hexdigit()), "uuid hex: {id}");
    }

    #[test]
    fn detect_anydoc_falls_back_to_npx_or_errors() {
        // Environment-dependent by design: node+npm present → the pinned
        // package via node's npx-cli.js; absent → structured Parse error.
        match detect_anydoc_command() {
            Ok(cmd) => {
                let tail = format!("{} {}", cmd.program, cmd.args.join(" "));
                assert!(
                    tail.contains("node") || tail.contains("anydoc"),
                    "unexpected command: {tail}"
                );
                if cmd.args.iter().any(|a| a == "@firecrawl/anydoc@0.1.7") {
                    assert!(
                        cmd.args.first().is_some_and(|a| a.ends_with("npx-cli.js")),
                        "npx fallback must run the npm npx entry through node, no shell: {cmd:?}"
                    );
                }
            }
            Err(e) => assert_eq!(e.code(), "PARSE"),
        }
    }
}
