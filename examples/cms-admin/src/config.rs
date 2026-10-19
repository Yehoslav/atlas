use std::env;
use std::net::SocketAddr;
use std::path::PathBuf;

use anyhow::{Context, Result, anyhow};

/// What the dashboard needs to start. Nothing here is optional except the
/// listen address, which has a localhost default so the example runs
/// without an .env file.
#[derive(Clone, Debug)]
pub struct Config {
    pub atlas_url: String,
    pub media_url: String,
    pub atlas_token: String,
    pub admin_password: String,
    pub cookie_secure: bool,
    pub listen: SocketAddr,
    pub templates_dir: PathBuf,
    pub static_dir: PathBuf,
}

impl Config {
    pub fn from_env() -> Result<Self> {
        let atlas_url =
            env::var("ATLAS_API_URL").unwrap_or_else(|_| "http://127.0.0.1:3000".to_owned());
        let media_url = env::var("ATLAS_MEDIA_URL").unwrap_or_else(|_| atlas_url.clone());
        let atlas_token = required("ATLAS_API_TOKEN")?;
        let admin_password = required("ADMIN_PASSWORD")?;
        let cookie_secure = env::var("ADMIN_COOKIE_SECURE")
            .map(|value| value == "1" || value.eq_ignore_ascii_case("true"))
            .unwrap_or(false);
        let listen = env::var("ADMIN_LISTEN")
            .unwrap_or_else(|_| "127.0.0.1:5001".to_owned())
            .parse()
            .context("ADMIN_LISTEN")?;

        // Next to the crate manifest is the natural place for templates and
        // static assets. `CARGO_MANIFEST_DIR` is set by Cargo at compile time,
        // so the example's layout is never a runtime configuration question.
        let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        let templates_dir = manifest.join("templates");
        let static_dir = manifest.join("static");

        Ok(Self {
            atlas_url,
            media_url: media_url.trim_end_matches('/').to_owned(),
            atlas_token,
            admin_password,
            cookie_secure,
            listen,
            templates_dir,
            static_dir,
        })
    }
}

fn required(name: &str) -> Result<String> {
    let value = env::var(name).map_err(|_| anyhow!("{name} is required"))?;
    if value.is_empty() {
        return Err(anyhow!("{name} must not be empty"));
    }
    Ok(value)
}
