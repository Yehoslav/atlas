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
    /// Load a local `.env` file, if there is one.
    ///
    /// Three places are searched, in order: `ADMIN_ENV_FILE` when it is set,
    /// the working directory, and this crate's own directory. The last one
    /// matters because the dashboard is usually started from the repository
    /// root with `--manifest-path`, and its settings live with the example
    /// rather than with whatever directory the command happened to run in.
    ///
    /// The file fills in what the environment does not already provide, so a
    /// real environment variable always wins.
    pub fn load_local_env() -> Result<()> {
        let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        let mut candidates = Vec::new();
        if let Ok(explicit) = env::var("ADMIN_ENV_FILE") {
            candidates.push(PathBuf::from(explicit));
        }
        candidates.push(PathBuf::from(".env"));
        candidates.push(manifest.join(".env"));
        for path in candidates {
            if path.is_file() {
                dotenvy::from_path(&path).with_context(|| format!("reading {}", path.display()))?;
            }
        }
        Ok(())
    }

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

#[cfg(test)]
mod tests {
    use super::Config;

    #[test]
    fn a_local_env_file_fills_in_what_the_environment_does_not_have() {
        // The environment is process-global, so this test owns it for its
        // duration and puts back what it changed.
        let original_token = std::env::var("ATLAS_API_TOKEN").ok();
        let original_password = std::env::var("ADMIN_PASSWORD").ok();
        let original_url = std::env::var("ATLAS_API_URL").ok();

        let directory = std::env::temp_dir().join("cms-admin-dotenv-test");
        std::fs::create_dir_all(&directory).unwrap();
        let path = directory.join(".env");
        std::fs::write(
            &path,
            "ATLAS_API_TOKEN=from-the-file\nADMIN_PASSWORD=from-the-file\n",
        )
        .unwrap();
        unsafe {
            std::env::set_var("ADMIN_ENV_FILE", &path);
            std::env::remove_var("ATLAS_API_TOKEN");
            std::env::remove_var("ADMIN_PASSWORD");
        }

        Config::load_local_env().unwrap();
        assert_eq!(std::env::var("ATLAS_API_TOKEN").unwrap(), "from-the-file");
        assert_eq!(std::env::var("ADMIN_PASSWORD").unwrap(), "from-the-file");

        // A real environment variable wins over the file.
        unsafe { std::env::set_var("ATLAS_API_TOKEN", "from-the-shell") };
        Config::load_local_env().unwrap();
        assert_eq!(std::env::var("ATLAS_API_TOKEN").unwrap(), "from-the-shell");

        unsafe {
            std::env::remove_var("ADMIN_ENV_FILE");
            match original_token {
                Some(value) => std::env::set_var("ATLAS_API_TOKEN", value),
                None => std::env::remove_var("ATLAS_API_TOKEN"),
            }
            match original_password {
                Some(value) => std::env::set_var("ADMIN_PASSWORD", value),
                None => std::env::remove_var("ADMIN_PASSWORD"),
            }
            match original_url {
                Some(value) => std::env::set_var("ATLAS_API_URL", value),
                None => std::env::remove_var("ATLAS_API_URL"),
            }
        }
        let _ = std::fs::remove_dir_all(&directory);
    }
}
