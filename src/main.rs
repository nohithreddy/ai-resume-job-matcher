use std::net::SocketAddr;

use resume_job_matcher::{build_application, config::AppConfig};
use tokio::net::TcpListener;
use tracing::info;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    // Ensure .env is loaded before any config/tracing reads env.
    let _ = dotenvy::dotenv();
    let config = AppConfig::from_env()?;
    tracing_subscriber::fmt()
        .with_env_filter(&config.log_filter)
        .json()
        .try_init()?;

    let address: SocketAddr = config.bind_address.parse()?;
    let listener = TcpListener::bind(address).await?;
    info!(%address, "resume-job-matcher listening");

    axum::serve(
        listener,
        build_application(config)?.into_make_service_with_connect_info::<SocketAddr>(),
    )
    .with_graceful_shutdown(shutdown_signal())
    .await?;
    Ok(())
}

async fn shutdown_signal() {
    let ctrl_c = async {
        if let Err(error) = tokio::signal::ctrl_c().await {
            tracing::error!(%error, "failed to install ctrl-c handler");
        }
    };

    #[cfg(unix)]
    let terminate = async {
        match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            Ok(mut signal) => {
                signal.recv().await;
            }
            Err(error) => tracing::error!(%error, "failed to install terminate handler"),
        }
    };

    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        _ = ctrl_c => {},
        _ = terminate => {},
    }
}
