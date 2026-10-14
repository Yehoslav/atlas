use std::{
    env, fs,
    path::{Path, PathBuf},
    time::Duration,
};

use crate::ident::{DEFAULT_LENGTH, MAX_LENGTH, MIN_LENGTH};

#[derive(Clone, Debug)]
pub struct Config {
    pub listen_addr: String,
    pub data_dir: PathBuf,
    pub api_token: String,
    pub id_length: usize,
    pub max_image_bytes: u64,
    pub max_document_bytes: u64,
    pub max_image_pixels: u64,
    pub max_image_dimension: u32,
    pub variant_widths: Vec<u32>,
    pub magick_bin: String,
    pub magick_timeout: Duration,
    pub image_processing_timeout: Duration,
    pub trash_retention_days: u32,
}

impl Config {
    pub fn from_env() -> Result<Self, String> {
        // The API token is deliberately excluded: secrets stay in the environment
        // and never live in a file that could be committed.
        let api_token = env::var("ATLAS_API_TOKEN")
            .map_err(|_| "ATLAS_API_TOKEN must be set to enable authenticated uploads")?;
        if api_token.trim().is_empty() {
            return Err("ATLAS_API_TOKEN must not be empty".to_owned());
        }

        let file = match env::var("ATLAS_CONFIG") {
            Ok(path) => FileConfig::load(Path::new(&path))?,
            Err(env::VarError::NotPresent) => FileConfig::default(),
            Err(error) => return Err(format!("cannot read ATLAS_CONFIG: {error}")),
        };

        let max_image_dimension = setting(
            &file.max_image_dimension,
            "ATLAS_MAX_IMAGE_DIMENSION",
            12_000,
            parse_u32,
        )?;
        let id_length = setting(
            &file.id_length,
            "ATLAS_ID_LENGTH",
            DEFAULT_LENGTH,
            parse_usize,
        )?;
        if !(MIN_LENGTH..=MAX_LENGTH).contains(&id_length) {
            return Err(format!(
                "ATLAS_ID_LENGTH must be between {MIN_LENGTH} and {MAX_LENGTH}"
            ));
        }
        let variant_widths = variant_widths(&file, max_image_dimension)?;

        Ok(Self {
            listen_addr: setting(
                &file.listen_addr,
                "ATLAS_LISTEN_ADDR",
                "127.0.0.1:3000".to_owned(),
                parse_string,
            )?,
            data_dir: setting(
                &file.data_dir,
                "ATLAS_DATA_DIR",
                PathBuf::from("./data"),
                parse_path,
            )?,
            api_token,
            id_length,
            max_image_bytes: positive(
                setting(
                    &file.max_image_bytes,
                    "ATLAS_MAX_IMAGE_BYTES",
                    20 * 1024 * 1024,
                    parse_u64,
                )?,
                "ATLAS_MAX_IMAGE_BYTES",
            )?,
            max_document_bytes: positive(
                setting(
                    &file.max_document_bytes,
                    "ATLAS_MAX_DOCUMENT_BYTES",
                    50 * 1024 * 1024,
                    parse_u64,
                )?,
                "ATLAS_MAX_DOCUMENT_BYTES",
            )?,
            max_image_pixels: positive(
                setting(
                    &file.max_image_pixels,
                    "ATLAS_MAX_IMAGE_PIXELS",
                    40_000_000,
                    parse_u64,
                )?,
                "ATLAS_MAX_IMAGE_PIXELS",
            )?,
            max_image_dimension,
            variant_widths,
            magick_bin: setting(
                &file.magick_bin,
                "ATLAS_MAGICK_BIN",
                "magick".to_owned(),
                parse_string,
            )?,
            magick_timeout: Duration::from_secs(positive(
                setting(
                    &file.magick_timeout_secs,
                    "ATLAS_MAGICK_TIMEOUT_SECS",
                    45,
                    parse_u64,
                )?,
                "ATLAS_MAGICK_TIMEOUT_SECS",
            )?),
            image_processing_timeout: Duration::from_secs(positive(
                setting(
                    &file.image_processing_timeout_secs,
                    "ATLAS_IMAGE_PROCESSING_TIMEOUT_SECS",
                    120,
                    parse_u64,
                )?,
                "ATLAS_IMAGE_PROCESSING_TIMEOUT_SECS",
            )?),
            trash_retention_days: u32::try_from(positive(
                setting(
                    &file.trash_retention_days,
                    "ATLAS_TRASH_RETENTION_DAYS",
                    30,
                    parse_u64,
                )?,
                "ATLAS_TRASH_RETENTION_DAYS",
            )?)
            .map_err(|_| "ATLAS_TRASH_RETENTION_DAYS must fit in 32 bits".to_owned())?,
        })
    }
}

#[derive(Default, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct FileConfig {
    listen_addr: Option<String>,
    data_dir: Option<PathBuf>,
    id_length: Option<usize>,
    max_image_bytes: Option<u64>,
    max_document_bytes: Option<u64>,
    max_image_pixels: Option<u64>,
    max_image_dimension: Option<u32>,
    variant_widths: Option<Vec<u32>>,
    magick_bin: Option<String>,
    magick_timeout_secs: Option<u64>,
    image_processing_timeout_secs: Option<u64>,
    trash_retention_days: Option<u64>,
}

impl FileConfig {
    fn load(path: &Path) -> Result<Self, String> {
        let contents = fs::read_to_string(path)
            .map_err(|error| format!("cannot read config file: {error}"))?;
        toml::from_str(&contents)
            .map_err(|error| format!("invalid config file {}: {error}", path.display()))
    }
}

/// Resolve one setting: the environment variable wins, then the config file,
/// then the built-in default.
fn setting<T: Clone>(
    from_file: &Option<T>,
    variable: &str,
    default: T,
    parse: fn(&str) -> Result<T, String>,
) -> Result<T, String> {
    match env::var(variable) {
        Ok(value) => parse(&value).map_err(|error| format!("{variable} {error}")),
        Err(env::VarError::NotPresent) => Ok(from_file.clone().unwrap_or(default)),
        Err(error) => Err(format!("cannot read {variable}: {error}")),
    }
}

fn variant_widths(file: &FileConfig, max_image_dimension: u32) -> Result<Vec<u32>, String> {
    let widths = match setting(
        &file.variant_widths,
        "ATLAS_VARIANT_WIDTHS",
        Vec::new(),
        |value| {
            value
                .split(',')
                .map(|item| item.trim().parse::<u32>())
                .collect::<Result<Vec<_>, _>>()
                .map_err(|_| {
                    "ATLAS_VARIANT_WIDTHS must be a comma-separated list of widths".to_owned()
                })
        },
    )? {
        widths if !widths.is_empty() => widths,
        _ => [320, 640, 960, 1280]
            .into_iter()
            .filter(|width| *width <= max_image_dimension)
            .collect(),
    };

    let mut widths = widths;
    widths.sort_unstable();
    widths.dedup();
    if widths.is_empty()
        || widths.len() > 8
        || widths
            .iter()
            .any(|width| *width == 0 || *width > max_image_dimension)
    {
        return Err(
            "ATLAS_VARIANT_WIDTHS must contain 1 to 8 positive widths within the image dimension limit"
                .to_owned(),
        );
    }
    Ok(widths)
}

fn parse_string(value: &str) -> Result<String, String> {
    Ok(value.to_owned())
}

fn parse_path(value: &str) -> Result<PathBuf, String> {
    Ok(PathBuf::from(value))
}

fn positive(value: u64, name: &str) -> Result<u64, String> {
    if value == 0 {
        return Err(format!("{name} must be greater than zero"));
    }
    Ok(value)
}

fn parse_u64(value: &str) -> Result<u64, String> {
    value
        .parse()
        .map_err(|_| "must be an unsigned integer".to_owned())
}

fn parse_u32(value: &str) -> Result<u32, String> {
    let value = parse_u64(value)?;
    u32::try_from(value).map_err(|_| "value is too large".to_owned())
}

fn parse_usize(value: &str) -> Result<usize, String> {
    value
        .parse()
        .map_err(|_| "must be a non-negative integer".to_owned())
}

#[cfg(test)]
mod tests {
    use std::sync::{Mutex, MutexGuard, OnceLock};

    use super::Config;

    fn env_lock() -> MutexGuard<'static, ()> {
        static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
        LOCK.get_or_init(|| Mutex::new(()))
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// SAFETY: every test that mutates the process environment holds `env_lock`,
    /// so no other thread can read or write these variables concurrently.
    fn set_var(name: &str, value: &str) {
        unsafe { std::env::set_var(name, value) };
    }

    /// SAFETY: see [`set_var`].
    fn remove_var(name: &str) {
        unsafe { std::env::remove_var(name) };
    }

    /// Environment variables are process-global, so tests that set them must
    /// not run concurrently.
    fn write_config(directory: &std::path::Path, contents: &str) -> std::path::PathBuf {
        let path = directory.join("atlas.toml");
        std::fs::write(&path, contents).unwrap();
        path
    }

    fn base_env() {
        set_var("ATLAS_API_TOKEN", "test-token");
        for name in [
            "ATLAS_LISTEN_ADDR",
            "ATLAS_DATA_DIR",
            "ATLAS_ID_LENGTH",
            "ATLAS_MAX_IMAGE_BYTES",
            "ATLAS_MAX_DOCUMENT_BYTES",
            "ATLAS_MAX_IMAGE_PIXELS",
            "ATLAS_MAX_IMAGE_DIMENSION",
            "ATLAS_VARIANT_WIDTHS",
            "ATLAS_MAGICK_BIN",
            "ATLAS_MAGICK_TIMEOUT_SECS",
            "ATLAS_IMAGE_PROCESSING_TIMEOUT_SECS",
            "ATLAS_TRASH_RETENTION_DAYS",
        ] {
            remove_var(name);
        }
        remove_var("ATLAS_CONFIG");
    }

    #[test]
    fn token_is_required() {
        let _guard = env_lock();
        base_env();
        remove_var("ATLAS_API_TOKEN");
        assert!(Config::from_env().is_err());
    }

    #[test]
    fn defaults_apply_without_config_file() {
        let _guard = env_lock();
        base_env();
        let config = Config::from_env().unwrap();
        assert_eq!(config.listen_addr, "127.0.0.1:3000");
        assert_eq!(config.id_length, super::DEFAULT_LENGTH);
        assert_eq!(config.variant_widths, vec![320, 640, 960, 1280]);
    }

    #[test]
    fn config_file_supplies_values() {
        let _guard = env_lock();
        base_env();
        let directory = tempfile::tempdir().unwrap();
        let path = write_config(
            directory.path(),
            r#"
            id_length = 20
            listen_addr = "0.0.0.0:9000"
            variant_widths = [400, 800]
            max_image_bytes = 1234
            "#,
        );
        set_var("ATLAS_CONFIG", &path.display().to_string());

        let config = Config::from_env().unwrap();
        assert_eq!(config.id_length, 20);
        assert_eq!(config.listen_addr, "0.0.0.0:9000");
        assert_eq!(config.variant_widths, vec![400, 800]);
        assert_eq!(config.max_image_bytes, 1234);
    }

    #[test]
    fn environment_overrides_config_file() {
        let _guard = env_lock();
        base_env();
        let directory = tempfile::tempdir().unwrap();
        let path = write_config(
            directory.path(),
            r#"
            id_length = 20
            listen_addr = "0.0.0.0:9000"
            "#,
        );
        set_var("ATLAS_CONFIG", &path.display().to_string());
        set_var("ATLAS_ID_LENGTH", "14");
        set_var("ATLAS_LISTEN_ADDR", "127.0.0.1:4321");

        let config = Config::from_env().unwrap();
        assert_eq!(config.id_length, 14);
        assert_eq!(config.listen_addr, "127.0.0.1:4321");
    }

    #[test]
    fn unknown_file_keys_are_rejected() {
        let _guard = env_lock();
        base_env();
        let directory = tempfile::tempdir().unwrap();
        let path = write_config(directory.path(), "api_token = \"leaked\"\n");
        set_var("ATLAS_CONFIG", &path.display().to_string());
        assert!(Config::from_env().is_err());
    }

    #[test]
    fn id_length_must_be_in_range() {
        let _guard = env_lock();
        base_env();
        for value in ["4", "64", "-1"] {
            set_var("ATLAS_ID_LENGTH", value);
            assert!(Config::from_env().is_err(), "accepted {value}");
        }
    }

    #[test]
    fn non_positive_limits_are_rejected() {
        let _guard = env_lock();
        base_env();
        set_var("ATLAS_MAX_IMAGE_BYTES", "0");
        assert!(Config::from_env().is_err());
    }

    #[test]
    fn malformed_numeric_environment_value_is_rejected() {
        let _guard = env_lock();
        base_env();
        set_var("ATLAS_MAX_IMAGE_PIXELS", "lots");
        let error = Config::from_env().unwrap_err();
        assert!(error.contains("ATLAS_MAX_IMAGE_PIXELS"), "{error}");
    }

    #[test]
    fn data_dir_comes_from_environment_or_file() {
        let _guard = env_lock();
        base_env();
        assert_eq!(
            Config::from_env().unwrap().data_dir,
            std::path::PathBuf::from("./data")
        );

        set_var("ATLAS_DATA_DIR", "/srv/atlas");
        assert_eq!(
            Config::from_env().unwrap().data_dir,
            std::path::PathBuf::from("/srv/atlas")
        );

        let directory = tempfile::tempdir().unwrap();
        let path = write_config(directory.path(), "data_dir = \"/var/lib/atlas\"\n");
        set_var("ATLAS_CONFIG", &path.display().to_string());

        remove_var("ATLAS_DATA_DIR");
        assert_eq!(
            Config::from_env().unwrap().data_dir,
            std::path::PathBuf::from("/var/lib/atlas")
        );

        set_var("ATLAS_DATA_DIR", "/srv/atlas");
        assert_eq!(
            Config::from_env().unwrap().data_dir,
            std::path::PathBuf::from("/srv/atlas")
        );
    }

    #[test]
    fn variant_widths_are_sorted_and_deduplicated() {
        let _guard = env_lock();
        base_env();
        set_var("ATLAS_VARIANT_WIDTHS", "800,400,800");
        let config = Config::from_env().unwrap();
        assert_eq!(config.variant_widths, vec![400, 800]);
    }

    #[test]
    fn trash_retention_defaults_to_thirty_days() {
        let _guard = env_lock();
        base_env();
        assert_eq!(Config::from_env().unwrap().trash_retention_days, 30);

        set_var("ATLAS_TRASH_RETENTION_DAYS", "7");
        assert_eq!(Config::from_env().unwrap().trash_retention_days, 7);

        set_var("ATLAS_TRASH_RETENTION_DAYS", "0");
        assert!(
            Config::from_env().is_err(),
            "zero days would purge instantly"
        );
    }
}
