//! Configuration management for Arctic Wolf NFS Server
//!
//! Loads configuration from:
//! 1. CLI argument `--config <path>` (if provided)
//! 2. Default path `/etc/arcticwolf/config.toml` (falls back to defaults if not found)

use clap::Parser;
use serde::Deserialize;
use std::path::PathBuf;

const DEFAULT_CONFIG_PATH: &str = "/etc/arcticwolf/config.toml";
const DEFAULT_QUOTA_DB_PATH: &str = "/var/lib/arcticwolf/quota.db";

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
    pub quota: QuotaConfig,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct ServerConfig {
    pub bind_address: String,
    pub nfs_port: u16,
    pub mount_port: u16,
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

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct QuotaConfig {
    /// Enable quota enforcement
    pub enabled: bool,
    /// Path to the redb database file storing quota limits and usage
    pub db_path: PathBuf,
}

impl Default for QuotaConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            db_path: PathBuf::from(DEFAULT_QUOTA_DB_PATH),
        }
    }
}

/// Parse a human-readable size string into bytes
///
/// Supported suffixes (case-insensitive): B, KB, MB, GB, TB
/// Uses 1024-based units (KiB/MiB/GiB/TiB semantics). Plain numbers are treated as bytes.
///
/// # Examples
/// - "1024" -> 1024
/// - "10KB" -> 10240
/// - "5MB" -> 5242880
/// - "2GB" -> 2147483648
#[allow(dead_code)]
pub fn parse_size(s: &str) -> anyhow::Result<u64> {
    let trimmed = s.trim();
    if trimmed.is_empty() {
        anyhow::bail!("Empty size string");
    }

    let upper = trimmed.to_uppercase();
    let (num_part, multiplier): (&str, u64) = if let Some(prefix) = upper.strip_suffix("TB") {
        (prefix, 1024u64.pow(4))
    } else if let Some(prefix) = upper.strip_suffix("GB") {
        (prefix, 1024u64.pow(3))
    } else if let Some(prefix) = upper.strip_suffix("MB") {
        (prefix, 1024u64.pow(2))
    } else if let Some(prefix) = upper.strip_suffix("KB") {
        (prefix, 1024)
    } else if let Some(prefix) = upper.strip_suffix('B') {
        (prefix, 1)
    } else {
        (upper.as_str(), 1)
    };

    let num_str = num_part.trim();
    let num: u64 = num_str
        .parse()
        .map_err(|e| anyhow::anyhow!("Invalid size '{}': {}", s, e))?;

    num.checked_mul(multiplier)
        .ok_or_else(|| anyhow::anyhow!("Size overflow: {}", s))
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            bind_address: "0.0.0.0".to_string(),
            nfs_port: 2049,
            mount_port: 0, // 0 = dynamic (OS-assigned)
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

    /// Get the bind address for a specific port
    pub fn bind_addr_for(&self, port: u16) -> String {
        format!("{}:{}", self.server.bind_address, port)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_server_config_default() {
        let config = ServerConfig::default();
        assert_eq!(config.bind_address, "0.0.0.0");
        assert_eq!(config.nfs_port, 2049);
        assert_eq!(config.mount_port, 0);
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
        assert_eq!(config.server.nfs_port, 2049);
        assert_eq!(config.server.mount_port, 0);
        assert_eq!(config.fsal.backend, "local");
        assert_eq!(config.fsal.export_path, PathBuf::from("/tmp/nfs_exports"));
        assert!(config.logging.level.is_none());
    }

    #[test]
    fn test_bind_addr_for() {
        let config = Config::default();
        assert_eq!(config.bind_addr_for(2049), "0.0.0.0:2049");
        assert_eq!(config.bind_addr_for(111), "0.0.0.0:111");

        let mut custom = Config::default();
        custom.server.bind_address = "127.0.0.1".to_string();
        assert_eq!(custom.bind_addr_for(2049), "127.0.0.1:2049");
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
            nfs_port = 2049
            mount_port = 20048

            [fsal]
            backend = "local"
            export_path = "/data/exports"

            [logging]
            level = "trace"
        "#;

        let config: Config = toml::from_str(toml).expect("Failed to parse TOML");
        assert_eq!(config.server.bind_address, "192.168.1.100");
        assert_eq!(config.server.nfs_port, 2049);
        assert_eq!(config.server.mount_port, 20048);
        assert_eq!(config.fsal.backend, "local");
        assert_eq!(config.fsal.export_path, PathBuf::from("/data/exports"));
        assert_eq!(config.logging.level, Some("trace".to_string()));
    }

    #[test]
    fn test_parse_partial_toml() {
        // Only specify server section, others should use defaults
        let toml = r#"
            [server]
            nfs_port = 8000
        "#;

        let config: Config = toml::from_str(toml).expect("Failed to parse TOML");
        assert_eq!(config.server.bind_address, "0.0.0.0"); // default
        assert_eq!(config.server.nfs_port, 8000); // custom
        assert_eq!(config.server.mount_port, 0); // default
        assert_eq!(config.fsal.backend, "local"); // default
        assert_eq!(config.fsal.export_path, PathBuf::from("/tmp/nfs_exports")); // default
        assert!(config.logging.level.is_none()); // default
    }

    #[test]
    fn test_parse_empty_toml() {
        let config: Config = toml::from_str("").expect("Failed to parse empty TOML");
        assert_eq!(config.server.bind_address, "0.0.0.0");
        assert_eq!(config.server.nfs_port, 2049);
        assert_eq!(config.server.mount_port, 0);
        assert_eq!(config.fsal.backend, "local");
    }

    #[test]
    fn test_parse_invalid_toml() {
        let result: Result<Config, _> = toml::from_str("this is not valid toml [[[");
        assert!(result.is_err());
    }

    #[test]
    fn test_quota_config_default() {
        let config = QuotaConfig::default();
        assert!(!config.enabled);
        assert_eq!(config.db_path, PathBuf::from(DEFAULT_QUOTA_DB_PATH));
    }

    #[test]
    fn test_config_default_includes_quota() {
        let config = Config::default();
        assert!(!config.quota.enabled);
    }

    #[test]
    fn test_parse_quota_toml() {
        let toml = r#"
            [quota]
            enabled = true
            db_path = "/tmp/quota.db"
        "#;

        let config: Config = toml::from_str(toml).expect("Failed to parse TOML");
        assert!(config.quota.enabled);
        assert_eq!(config.quota.db_path, PathBuf::from("/tmp/quota.db"));
    }

    #[test]
    fn test_parse_size_bytes() {
        assert_eq!(parse_size("0").unwrap(), 0);
        assert_eq!(parse_size("1").unwrap(), 1);
        assert_eq!(parse_size("1024").unwrap(), 1024);
        assert_eq!(parse_size("512B").unwrap(), 512);
    }

    #[test]
    fn test_parse_size_kb() {
        assert_eq!(parse_size("1KB").unwrap(), 1024);
        assert_eq!(parse_size("10KB").unwrap(), 10 * 1024);
        assert_eq!(parse_size("1kb").unwrap(), 1024); // case-insensitive
    }

    #[test]
    fn test_parse_size_mb() {
        assert_eq!(parse_size("1MB").unwrap(), 1024 * 1024);
        assert_eq!(parse_size("5MB").unwrap(), 5 * 1024 * 1024);
    }

    #[test]
    fn test_parse_size_gb() {
        assert_eq!(parse_size("1GB").unwrap(), 1024u64.pow(3));
        assert_eq!(parse_size("10GB").unwrap(), 10 * 1024u64.pow(3));
    }

    #[test]
    fn test_parse_size_tb() {
        assert_eq!(parse_size("1TB").unwrap(), 1024u64.pow(4));
        assert_eq!(parse_size("2TB").unwrap(), 2 * 1024u64.pow(4));
    }

    #[test]
    fn test_parse_size_whitespace() {
        assert_eq!(parse_size("  10MB  ").unwrap(), 10 * 1024 * 1024);
    }

    #[test]
    fn test_parse_size_invalid() {
        assert!(parse_size("").is_err());
        assert!(parse_size("abc").is_err());
        assert!(parse_size("10XB").is_err());
        assert!(parse_size("-5MB").is_err());
    }

    #[test]
    fn test_parse_size_overflow() {
        // u64::MAX / 1024^4 is ~16 million TB
        assert!(parse_size("99999999999999999TB").is_err());
    }
}
