mod api;
mod files;
mod lsp;
mod terminal;
mod watch;

use std::path::PathBuf;

use anyhow::Context;
use axum::Router;
use clap::Parser;
use tower_http::{
    cors::CorsLayer,
    services::{ServeDir, ServeFile},
    trace::TraceLayer,
};
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt};

#[derive(Debug, Parser)]
#[command(name = "oxidize")]
#[command(about = "A lightweight browser-based development environment.")]
struct Args {
    /// Folder or file to open on launch.
    path: Option<PathBuf>,

    #[arg(long, default_value = "127.0.0.1")]
    host: String,

    #[arg(long, default_value_t = 7878)]
    port: u16,

    #[arg(long, default_value = "frontend/dist")]
    static_dir: PathBuf,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::registry()
        .with(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "oxidize_server=info,tower_http=info".into()),
        )
        .with(tracing_subscriber::fmt::layer())
        .init();

    let args = Args::parse();
    if args.host.trim().is_empty() {
        anyhow::bail!("--host cannot be empty");
    }
    if args.port == 0 {
        anyhow::bail!("invalid port: 0");
    }
    if args.static_dir.as_os_str().is_empty() {
        anyhow::bail!("--static-dir cannot be empty");
    }
    let initial_workspace = args
        .path
        .as_deref()
        .map(files::open_workspace)
        .transpose()
        .context("failed to open initial workspace")?;

    let index_file = args.static_dir.join("index.html");
    let static_files = ServeDir::new(&args.static_dir)
        .append_index_html_on_directories(true)
        .fallback(ServeFile::new(index_file));
    let app = Router::new()
        .nest("/api", api::router(initial_workspace))
        .fallback_service(static_files)
        .layer(CorsLayer::permissive())
        .layer(TraceLayer::new_for_http());

    let address = tokio::net::lookup_host((args.host.as_str(), args.port))
        .await
        .context("invalid bind address")?
        .next()
        .context("failed to resolve bind address")?;
    let listener = tokio::net::TcpListener::bind(address).await?;
    tracing::info!("Oxidize is available at http://{address}");

    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await?;
    Ok(())
}

async fn shutdown_signal() {
    let ctrl_c = async {
        tokio::signal::ctrl_c()
            .await
            .expect("failed to install Ctrl+C handler");
    };

    #[cfg(unix)]
    let terminate = async {
        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .expect("failed to install termination signal handler")
            .recv()
            .await;
    };

    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        _ = ctrl_c => {},
        _ = terminate => {},
    }
}
