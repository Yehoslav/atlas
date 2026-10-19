mod atlas;
mod auth;
mod config;
mod web;

use anyhow::{Context, Result};
use tokio::net::TcpListener;

use crate::atlas::Atlas;
use crate::auth::Auth;
use crate::config::Config;
use crate::web::{AppState, router};

#[tokio::main]
async fn main() -> Result<()> {
    // A local `.env` file, if there is one, before anything reads the
    // environment. Its values only fill in what the environment does not
    // already provide.
    Config::load_local_env()?;

    let config = Config::from_env()?;
    let atlas = Atlas::new(&config.atlas_url, &config.atlas_token)?;
    let auth = Auth::new(config.admin_password, config.cookie_secure);
    let state = AppState::new(atlas, auth, config.media_url, config.templates_dir)?;
    let app = router(state, config.static_dir);
    let listener = TcpListener::bind(config.listen)
        .await
        .with_context(|| format!("binding dashboard to {}", config.listen))?;
    eprintln!(
        "CMS admin dashboard listening on http://{}",
        listener.local_addr()?
    );
    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await
        .context("serving the dashboard")?;
    Ok(())
}

async fn shutdown_signal() {
    let ctrl_c = async {
        tokio::signal::ctrl_c()
            .await
            .expect("install Ctrl-C handler");
    };

    #[cfg(unix)]
    let terminate = async {
        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .expect("install SIGTERM handler")
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
