//! Runs the gateway without the desktop shell, for use as a plain local service.
use gateway_server::state::AppState;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info,tower_http=debug".into()),
        )
        .init();

    let path = gateway_server::config_path()?;
    let state = AppState::load(path.clone())?;
    tracing::info!("using configuration {}", path.display());

    gateway_server::serve(state).await?;
    Ok(())
}
