//! Boots the streamable-HTTP MCP server and publishes its connect URL to the
//! daemon lockfile. Run by `daemon begin`; the single daemon-hosted server is
//! shared by every agent.

use std::sync::Arc;
use std::time::Duration;

use rmcp::transport::streamable_http_server::{
    StreamableHttpServerConfig, StreamableHttpService, session::local::LocalSessionManager,
};

use super::ArcanumMcp;
use crate::context::Context;
use crate::monitor::MonitorService;

/// Run the MCP server (and the token-usage monitor service) until process death.
///
/// Binds an OS-assigned port on loopback (`127.0.0.1:0`), publishes the
/// resulting `http://<addr>` into `<state_dir>/locks` under key `"mcp"` for the
/// launcher (`mcp arcanum begin`) to discover, then serves. Produces no
/// stdout/stderr — the lockfile is the only side channel. Alongside the server
/// it runs a Postgres `LISTEN` loop that (re)starts a per-agent monitor whenever
/// `mcp arcanum begin` NOTIFYs.
pub async fn run(ctx: Arc<Context>) -> std::io::Result<()> {
    let lock_dir = ctx.config.state_dir().join("locks");

    // Connect the DB and build the monitor service (shared with the server).
    let db = ctx.db().await?.clone();
    let monitor = MonitorService::new(db.clone(), ctx.executor.clone());
    spawn_monitor_listener(db, monitor.clone());

    let server = ArcanumMcp::new(ctx, monitor);
    let service = StreamableHttpService::new(
        move || Ok(server.clone()),
        Arc::new(LocalSessionManager::default()),
        {
            // `StreamableHttpServerConfig` is `#[non_exhaustive]`, so mutate a
            // default rather than constructing it with a struct literal.
            // Stateful: the host's MCP proxy requires the `Mcp-Session-Id` header.
            let mut cfg = StreamableHttpServerConfig::default();
            cfg.stateful_mode = true;
            cfg.sse_keep_alive = None;
            cfg
        },
    );

    let router = axum::Router::new().fallback_service(service);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;

    // Publish the connect URL for the launcher, mapping an unspecified bind to
    // loopback. The `LockClaim` is held until process death (it leaks on drop by
    // design); we only check for a conflicting live holder.
    let addr = listener.local_addr()?;
    let connect_ip = match addr.ip() {
        std::net::IpAddr::V4(v4) if v4.is_unspecified() => {
            std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST)
        }
        std::net::IpAddr::V6(v6) if v6.is_unspecified() => {
            std::net::IpAddr::V6(std::net::Ipv6Addr::LOCALHOST)
        }
        ip => ip,
    };
    let connect_url = format!("http://{}", std::net::SocketAddr::new(connect_ip, addr.port()));
    if objectiveai_sdk::lockfile::try_acquire(&lock_dir, "mcp", &connect_url)
        .await
        .is_none()
    {
        return Err(std::io::Error::other(
            "another arcanum instance already holds the mcp lock for this state",
        ));
    }

    axum::serve(listener, router).await
}

/// The `arcanum_monitor` NOTIFY payload: which AIH to (re)evaluate and its
/// `token_repeat` (which isn't persisted).
#[derive(serde::Deserialize)]
struct MonitorNotification {
    aih: String,
    token_repeat: i64,
}

/// Background task: LISTEN on the `arcanum_monitor` channel and, for each
/// notified AIH, run the begin trigger — (re)start its monitor loop, but only if
/// a skill is loaded (see [`MonitorService::on_begin`]). Reconnects on error.
fn spawn_monitor_listener(db: crate::db::Db, monitor: Arc<MonitorService>) {
    tokio::spawn(async move {
        loop {
            let mut listener = match db.monitor_listener().await {
                Ok(l) => l,
                Err(_) => {
                    tokio::time::sleep(Duration::from_secs(1)).await;
                    continue;
                }
            };
            // Drain notifications until the connection drops, then reconnect.
            while let Ok(notification) = listener.recv().await {
                if let Ok(n) = serde_json::from_str::<MonitorNotification>(notification.payload()) {
                    monitor.on_begin(&n.aih, n.token_repeat).await;
                }
            }
        }
    });
}
