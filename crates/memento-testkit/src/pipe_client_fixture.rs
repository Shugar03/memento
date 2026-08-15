//! Single-purpose pipe client fixture for tests that only need a
//! [`DaemonClient`](memento_cli::transport::DaemonClient) (daemon-persistent
//! B7, design D10).
//!
//! [`PipeClientFixture::connect`] wraps [`DaemonFixture::start`] +
//! [`DaemonFixture::connect_client`] into a one-liner. Tests that only
//! exercise the wire (no dispatcher introspection, no state inspection)
//! should prefer this fixture — it hides the two-step dance behind a
//! single constructor.
//!
//! The fixture returns both halves: the live [`DaemonFixture`] (for
//! shutdown + cleanup) and the connected `DaemonClient` (for assertions).

use std::path::{Path, PathBuf};
use std::time::Duration;

use memento_cli::transport::pipe_client::{ClientConfig, DaemonClient};
use memento_domain::{TenantContext, TenantId};

use crate::daemon_fixture::{DaemonFixture, DaemonFixtureOptions};

/// A connected `DaemonClient` paired with the [`DaemonFixture`] that owns
/// its daemon. Drop both halves at the end of the test.
pub struct PipeClientFixture {
    /// The in-process daemon (drop it to shut the accept loop).
    pub daemon: DaemonFixture,
    /// A connected CLI-side `DaemonClient` (drop it to release the
    /// connection — the daemon keeps serving, REQ-DAEMON-006).
    pub client: DaemonClient,
}

impl PipeClientFixture {
    /// Start an in-process daemon against `root` + `ctx`, then connect a
    /// single CLI-side `DaemonClient`. The cookie file is written by the
    /// fixture; the token is the literal the test supplies.
    pub async fn connect(
        root: PathBuf,
        ctx: TenantContext,
        token: String,
        no_embeddings: bool,
        locale: Option<String>,
    ) -> Self {
        let opts = DaemonFixtureOptions {
            root: root.clone(),
            ctx: ctx.clone(),
            token: token.clone(),
            no_embeddings,
            locale: locale.clone(),
            pipe_timeout: Duration::from_secs(2),
        };
        let daemon = DaemonFixture::start(opts).await;
        // The CLI-side client resolves its config from env. We construct
        // it manually here so the fixture is hermetic (no env mutation,
        // no `MEMENTO_TOKEN` leak between tests).
        let tenant_id_str = ctx.tenant_id().to_string();
        let config = ClientConfig {
            root: root.clone(),
            token,
            agent_id: ctx.agent_id().to_string(),
            tenant_id: tenant_id_str,
            locale,
            no_embeddings,
            pipe_timeout: Duration::from_secs(2),
        };
        let client = DaemonClient::connect(&config)
            .await
            .expect("DaemonClient connects to fixture daemon");
        Self { daemon, client }
    }

    /// Convenience: `connect` with the test's `tempfile::TempDir` + the
    /// canonical `TenantContext::new_for_tests` tenant identity.
    pub async fn for_tempstore(root: &Path, ctx: TenantContext, no_embeddings: bool) -> Self {
        let tenant_id = *ctx.tenant_id();
        Self::connect(
            root.to_path_buf(),
            ctx,
            format!("memo_test_{tenant_id}"),
            no_embeddings,
            None,
        )
        .await
    }

    /// The fixture's storage root (production layout root).
    pub fn root(&self) -> &Path {
        self.daemon.root()
    }

    /// The bound tenant id.
    pub fn tenant_id(&self) -> &TenantId {
        self.daemon.tenant_id()
    }

    /// The cookie nonce path the daemon wrote at startup.
    pub fn cookie_path(&self) -> &Path {
        self.daemon.cookie_path()
    }
}
