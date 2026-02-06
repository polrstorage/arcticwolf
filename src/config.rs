//! Configuration management for Arctic Wolf NFS Server
//!
//! Loads configuration from:
//! 1. CLI argument `--config <path>` (if provided)
//! 2. Default path `/etc/arcticwolf/config.toml` (falls back to defaults if not found)

use clap::Parser;
use serde::Deserialize;
use std::path::PathBuf;

const DEFAULT_CONFIG_PATH: &str = "/etc/arcticwolf/config.toml";

#[derive(Parser, Debug)]
#[command(name = "arcticwolf")]
#[command(about = "Arctic Wolf NFS Server", long_about = None)]
pub struct Cli {
    /// Path to configuration file
    #[arg(short, long)]
    pub config: Option<PathBuf>,
}

#[derive(Debug, Clone, Deserialize, Default)]
#[serde(default)]
pub struct Config {
    pub server: ServerConfig,
    pub fsal: FsalConfig,
    pub logging: LoggingConfig,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct ServerConfig {
    pub bind_address: String,
    pub port: u16,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct FsalConfig {
    pub backend: String,
    pub export_path: PathBuf,
}

#[derive(Debug, Clone, Deserialize, Default)]
pub struct LoggingConfig {
    /// Log level. If not set, falls back to RUST_LOG env var, then "info"
    pub level: Option<String>,
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            bind_address: "0.0.0.0".to_string(),
            port: 4000,
        }
    }
}

impl Default for FsalConfig {
    fn default() -> Self {
        Self {
            backend: "local".to_string(),
            export_path: PathBuf::from("/tmp/nfs_exports"),
        }
    }
}

impl LoggingConfig {
    /// Get log level with fallback: config -> RUST_LOG -> "info"
    pub fn effective_level(&self) -> String {
        match self.level.as_deref() {
            Some(level) => level.to_string(),
            None => std::env::var("RUST_LOG").unwrap_or_else(|_| "info".to_string()),
        }
    }
}

impl Config {
    /// Load configuration from file or use defaults
    pub fn load() -> anyhow::Result<Self> {
        let cli = Cli::parse();

        let (config_path, user_specified) = match cli.config {
            Some(path) => (path, true),
            None => (PathBuf::from(DEFAULT_CONFIG_PATH), false),
        };

        if config_path.exists() {
            let content = std::fs::read_to_string(&config_path)?;
            let config: Config = toml::from_str(&content)?;
            println!("  Config: {}", config_path.display());
            Ok(config)
        } else if user_specified {
            // User specified --config but file doesn't exist
            anyhow::bail!("Configuration file not found: {}", config_path.display());
        } else {
            // Default path doesn't exist, use defaults
            println!("  Config: using defaults");
            Ok(Config::default())
        }
    }

    /// Get the server bind address with port
    pub fn bind_addr(&self) -> String {
        format!("{}:{}", self.server.bind_address, self.server.port)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_server_config_default() {
        let config = ServerConfig::default();
        assert_eq!(config.bind_address, "0.0.0.0");
        assert_eq!(config.port, 4000);
    }

    #[test]
    fn test_fsal_config_default() {
        let config = FsalConfig::default();
        assert_eq!(config.backend, "local");
        assert_eq!(config.export_path, PathBuf::from("/tmp/nfs_exports"));
    }

    #[test]
    fn test_logging_config_default() {
        let config = LoggingConfig::default();
        assert!(config.level.is_none());
    }

    #[test]
    fn test_config_default() {
        let config = Config::default();
        assert_eq!(config.server.bind_address, "0.0.0.0");
        assert_eq!(config.server.port, 4000);
        assert_eq!(config.fsal.backend, "local");
        assert_eq!(config.fsal.export_path, PathBuf::from("/tmp/nfs_exports"));
        assert!(config.logging.level.is_none());
    }

    #[test]
    fn test_bind_addr() {
        let config = Config::default();
        assert_eq!(config.bind_addr(), "0.0.0.0:4000");

        let mut custom = Config::default();
        custom.server.bind_address = "127.0.0.1".to_string();
        custom.server.port = 2049;
        assert_eq!(custom.bind_addr(), "127.0.0.1:2049");
    }

    #[test]
    fn test_effective_level_with_config() {
        let config = LoggingConfig {
            level: Some("debug".to_string()),
        };
        assert_eq!(config.effective_level(), "debug");
    }

    #[test]
    fn test_effective_level_fallback() {
        // Determine expected level based on current environment without mutating it
        let expected = std::env::var("RUST_LOG").unwrap_or_else(|_| "info".to_string());
        let config = LoggingConfig { level: None };
        assert_eq!(config.effective_level(), expected);
    }

    #[test]
    fn test_parse_full_toml() {
        let toml = r#"
            [server]
            bind_address = "192.168.1.100"
            port = 2049

            [fsal]
            backend = "local"
            export_path = "/data/exports"

            [logging]
            level = "trace"
        "#;

        let config: Config = toml::from_str(toml).expect("Failed to parse TOML");
        assert_eq!(config.server.bind_address, "192.168.1.100");
        assert_eq!(config.server.port, 2049);
        assert_eq!(config.fsal.backend, "local");
        assert_eq!(config.fsal.export_path, PathBuf::from("/data/exports"));
        assert_eq!(config.logging.level, Some("trace".to_string()));
    }

    #[test]
    fn test_parse_partial_toml() {
        // Only specify server section, others should use defaults
        let toml = r#"
            [server]
            port = 8000
        "#;

        let config: Config = toml::from_str(toml).expect("Failed to parse TOML");
        assert_eq!(config.server.bind_address, "0.0.0.0"); // default
        assert_eq!(config.server.port, 8000); // custom
        assert_eq!(config.fsal.backend, "local"); // default
        assert_eq!(config.fsal.export_path, PathBuf::from("/tmp/nfs_exports")); // default
        assert!(config.logging.level.is_none()); // default
    }

    #[test]
    fn test_parse_empty_toml() {
        let config: Config = toml::from_str("").expect("Failed to parse empty TOML");
        assert_eq!(config.server.bind_address, "0.0.0.0");
        assert_eq!(config.server.port, 4000);
        assert_eq!(config.fsal.backend, "local");
    }

    #[test]
    fn test_parse_invalid_toml() {
        let result: Result<Config, _> = toml::from_str("this is not valid toml [[[");
        assert!(result.is_err());
    }
}
