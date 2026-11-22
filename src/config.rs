use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

/// On-disk configuration.
///
/// The token is stored in a file with `0600` permissions on Unix. Readeck API
/// tokens are long-lived credentials, so the configuration file is the only
/// place this application ever persists one.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    #[serde(default)]
    pub server: String,
    #[serde(default)]
    pub token: String,
    #[serde(default)]
    pub theme: String,
    #[serde(default = "default_images")]
    pub images: bool,
    #[serde(default)]
    pub terminal_graphics: bool,
    /// Maximum article width before wrapping; 0 uses the full viewport.
    #[serde(default)]
    pub text_width: usize,
    /// Reading column alignment: `center` (default) or `left`.
    #[serde(default = "default_text_align")]
    pub text_align: String,
    /// Speed reading pace in words per minute.
    #[serde(default = "default_speed_read_wpm")]
    pub speed_read_wpm: u16,
}

fn default_text_align() -> String {
    "center".to_string()
}

fn default_speed_read_wpm() -> u16 {
    crate::speed::DEFAULT_WPM
}

fn default_images() -> bool {
    true
}

impl Default for Config {
    fn default() -> Self {
        Self {
            server: String::new(),
            token: String::new(),
            theme: String::new(),
            images: true,
            terminal_graphics: false,
            text_width: 0,
            text_align: default_text_align(),
            speed_read_wpm: default_speed_read_wpm(),
        }
    }
}

impl Config {
    pub fn has_credentials(&self) -> bool {
        !self.server.trim().is_empty() && !self.token.trim().is_empty()
    }

    /// Load a configuration file. A missing file is not an error.
    pub fn load(path: &Path) -> Result<Option<Self>> {
        if !path.exists() {
            return Ok(None);
        }
        let data = fs::read_to_string(path).with_context(|| {
            format!("could not read config file {}", path.display())
        })?;
        let config = toml::from_str(&data).with_context(|| {
            format!("could not parse config file {}", path.display())
        })?;
        Ok(Some(config))
    }

    pub fn save(&self, path: &Path) -> Result<()> {
        let parent = path.parent().unwrap_or_else(|| Path::new("."));
        fs::create_dir_all(parent).with_context(|| {
            format!("could not create directory {}", parent.display())
        })?;

        let data = toml::to_string_pretty(self)
            .context("could not serialize configuration")?;
        fs::write(path, data).with_context(|| {
            format!("could not write config file {}", path.display())
        })?;

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut perms = fs::metadata(path)?.permissions();
            perms.set_mode(0o600);
            fs::set_permissions(path, perms)?;
        }

        Ok(())
    }
}

/// Default location: `$XDG_CONFIG_HOME/eckly/config.toml` (or the platform
/// equivalent).
pub fn default_path() -> PathBuf {
    dirs::config_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("eckly")
        .join("config.toml")
}

/// Normalize a user supplied instance URL.
///
/// Accepts `host:port`, adds `http://` when no scheme is present, strips
/// trailing slashes and a trailing `/api` path so endpoints can be appended
/// consistently.
pub fn normalize_server(input: &str) -> Result<String> {
    let mut server = input.trim().to_string();
    if server.is_empty() {
        anyhow::bail!("server URL is empty");
    }
    if !server.contains("://") {
        server = format!("http://{server}");
    }
    let parsed = url::Url::parse(&server).context("invalid server URL")?;
    match parsed.scheme() {
        "http" | "https" => {}
        scheme => anyhow::bail!(
            "unsupported URL scheme `{scheme}` (use http or https)"
        ),
    }
    if parsed.host_str().is_none() {
        anyhow::bail!("server URL has no host");
    }
    if parsed.query().is_some() || parsed.fragment().is_some() {
        anyhow::bail!("server URL must not contain a query or fragment");
    }

    let mut normalized = parsed.as_str().trim_end_matches('/').to_string();
    if normalized.ends_with("/api") {
        normalized.truncate(normalized.len() - 4);
    }
    Ok(normalized)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn images_default_to_enabled() {
        let config: Config = toml::from_str("").unwrap();
        assert!(config.images);

        let config: Config = toml::from_str("images = false").unwrap();
        assert!(!config.images);
        assert_eq!(config.text_width, 0);

        let config: Config = toml::from_str("text_width = 80").unwrap();
        assert_eq!(config.text_width, 80);
        assert_eq!(config.text_align, "center");

        let config: Config = toml::from_str("text_align = \"left\"").unwrap();
        assert_eq!(config.text_align, "left");

        let config: Config = toml::from_str("").unwrap();
        assert_eq!(config.speed_read_wpm, 300);
        let config: Config = toml::from_str("speed_read_wpm = 450").unwrap();
        assert_eq!(config.speed_read_wpm, 450);
    }

    #[test]
    fn terminal_graphics_default_to_off() {
        let config: Config = toml::from_str("").unwrap();
        assert!(!config.terminal_graphics);

        let config: Config =
            toml::from_str("terminal_graphics = true").unwrap();
        assert!(config.terminal_graphics);
    }

    #[test]
    fn normalizes_servers() {
        assert_eq!(
            normalize_server("readeck.example.com").unwrap(),
            "http://readeck.example.com"
        );
        assert_eq!(
            normalize_server("https://readeck.example.com/").unwrap(),
            "https://readeck.example.com"
        );
        assert_eq!(
            normalize_server(" https://example.com/readeck/api/ ").unwrap(),
            "https://example.com/readeck"
        );
        assert!(normalize_server("ftp://example.com").is_err());
        assert!(normalize_server("").is_err());
        assert!(normalize_server("https://example.com/?x=1").is_err());
    }
}
