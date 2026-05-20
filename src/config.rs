//! Configuration management for Arctic Wolf NFS Server
//!
//! Loads configuration from:
//! 1. CLI argument `--config <path>` (if provided)
//! 2. Default path `/etc/arcticwolf/config.toml` (falls back to defaults if not found)

use anyhow::bail;
use clap::Parser;
use serde::Deserialize;
use std::collections::{HashMap, HashSet};
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

#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    pub server: ServerConfig,
    pub exports: Vec<ExportConfig>,
    pub logging: LoggingConfig,
    pub admin: AdminConfig,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct ServerConfig {
    pub bind_address: String,
    pub nfs_port: u16,
    pub mount_port: u16,
}

/// Configuration for a single NFS export.
///
/// Note: cannot use `#[serde(deny_unknown_fields)]` on this struct because the
/// `backend` field is `#[serde(flatten)]` and serde rejects the combination
/// (fields belonging to the flattened `BackendConfig` would get reported as
/// unknown at this level). Typos at the export level are still rejected,
/// just by a cascade rather than directly here: serde routes any key not
/// matched by `ExportConfig`'s direct fields through the flatten buffer into
/// `BackendConfig`, whose own `deny_unknown_fields` errors out — so e.g.
/// `readOnly = true` instead of `read_only = true` fails with
/// "unknown field `readOnly`, expected `path`". For non-flattened sections
/// like `[server]` and `[logging]`, `Config::load` additionally wraps
/// deserialization with `serde_ignored` to catch typos there as well.
#[derive(Debug, Clone, Deserialize)]
pub struct ExportConfig {
    /// Export path as advertised to NFS clients (e.g. "/data"). Must start with `/`.
    pub name: String,
    /// Unique non-zero export identifier used to derive file handles.
    pub uid: u32,
    /// If true, deny writes against this export.
    #[serde(default)]
    pub read_only: bool,
    /// Backend-specific configuration. Discriminated by the `backend` field.
    #[serde(flatten)]
    pub backend: BackendConfig,
}

/// Storage backend selection for an export.
///
/// Tagged union — the `backend` key in TOML selects the variant and the remaining
/// keys deserialize into that variant's fields.
#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "backend", rename_all = "lowercase", deny_unknown_fields)]
pub enum BackendConfig {
    Local { path: PathBuf },
}

impl BackendConfig {
    /// Short identifier for the active backend variant, suitable for log/banner output.
    pub fn name(&self) -> &'static str {
        match self {
            BackendConfig::Local { .. } => "local",
        }
    }
}

#[derive(Debug, Clone, Deserialize, Default)]
pub struct LoggingConfig {
    /// Log level. If not set, falls back to RUST_LOG env var, then "info"
    pub level: Option<String>,
}

/// Admin Unix-domain-socket server settings.
///
/// The admin transport (issue #25) is opt-in: by default `enabled = false`
/// so the scaffolding is inert and existing deployments are unaffected.
/// When enabled, the daemon binds a length-prefixed JSON server at
/// `socket_path` and applies `socket_mode` (default `0o600`).
#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct AdminConfig {
    /// Whether the admin server is started at all. Defaults to `false`.
    pub enabled: bool,
    /// Filesystem path for the admin Unix domain socket.
    pub socket_path: PathBuf,
    /// File mode applied to the socket via `chmod(2)` after bind.
    ///
    /// TOML doesn't have an octal literal, so values are written in either
    /// decimal (e.g. `384`) or Rust-style octal (e.g. `0o600`) — both
    /// deserialize through `u32` here. We don't try to validate the mode
    /// bits beyond their range; the kernel will reject anything wider when
    /// `chmod` is called.
    pub socket_mode: u32,
}

impl Default for AdminConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            socket_path: PathBuf::from(crate::admin::DEFAULT_ADMIN_SOCKET_PATH),
            socket_mode: 0o600,
        }
    }
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

impl Default for Config {
    fn default() -> Self {
        Self {
            server: ServerConfig::default(),
            exports: vec![ExportConfig {
                name: "/".to_string(),
                uid: 1,
                read_only: false,
                backend: BackendConfig::Local {
                    path: PathBuf::from("/tmp/nfs_exports"),
                },
            }],
            logging: LoggingConfig::default(),
            admin: AdminConfig::default(),
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

        let config = if config_path.exists() {
            let content = std::fs::read_to_string(&config_path)?;
            // `Config` and `BackendConfig` carry `deny_unknown_fields`, but
            // `ExportConfig` can't (it uses `#[serde(flatten)]`). Wrap the
            // deserializer in `serde_ignored` so any key serde would have
            // silently dropped at the export level is surfaced as an error.
            let mut unknown_keys: Vec<String> = Vec::new();
            let de = toml::Deserializer::new(&content);
            let config: Config = serde_ignored::deserialize(de, |path| {
                unknown_keys.push(path.to_string());
            })?;
            if !unknown_keys.is_empty() {
                bail!(
                    "Unknown configuration key(s) in {}: {}",
                    config_path.display(),
                    unknown_keys.join(", ")
                );
            }
            println!("  Config: {}", config_path.display());
            config
        } else if user_specified {
            // User specified --config but file doesn't exist
            bail!("Configuration file not found: {}", config_path.display());
        } else {
            // Default path doesn't exist, use defaults
            println!("  Config: using defaults");
            Config::default()
        };

        config.validate()?;
        Ok(config)
    }

    /// Validate that the loaded configuration describes a usable server.
    ///
    /// Enforces invariants the rest of the code relies on:
    /// - at least one export is defined
    /// - every export `uid` is non-zero (uid 0 is reserved)
    /// - export `uid`s are unique (collisions would make file handles ambiguous)
    /// - export `name`s are unique and start with `/`
    pub fn validate(&self) -> anyhow::Result<()> {
        if self.exports.is_empty() {
            bail!("Configuration must define at least one [[exports]] entry");
        }

        // Reject mode values that overflow the chmod(2) permission bits.
        // `chmod` silently masks anything above 0o777, so accepting e.g.
        // `0o7777` would lead to a surprising "I set 7777 but stat shows
        // 0777" mismatch. Validating up front gives the operator a clear
        // error pointing at the field they got wrong.
        if self.admin.socket_mode > 0o777 {
            bail!(
                "admin.socket_mode = {:o} is invalid; must be <= 0o777 (chmod silently masks higher bits)",
                self.admin.socket_mode,
            );
        }

        let mut seen_uids: HashMap<u32, &str> = HashMap::new();
        let mut seen_names: HashSet<&str> = HashSet::new();

        for export in &self.exports {
            if export.uid == 0 {
                bail!(
                    "Export '{}' has uid 0; uid must be a non-zero u32",
                    export.name
                );
            }
            if let Some(first_name) = seen_uids.get(&export.uid) {
                bail!(
                    "Duplicate export uid {}: first used by '{}', conflict on '{}'",
                    export.uid,
                    first_name,
                    export.name
                );
            }
            seen_uids.insert(export.uid, export.name.as_str());
            if !export.name.starts_with('/') {
                bail!(
                    "Export name '{}' must start with '/' (e.g. '/data')",
                    export.name
                );
            }
            if !seen_names.insert(export.name.as_str()) {
                bail!("Duplicate export name '{}'", export.name);
            }
            // Backend-specific validation.
            match &export.backend {
                BackendConfig::Local { path } => {
                    if !path.is_absolute() {
                        bail!(
                            "Export '{}' local backend path '{}' must be absolute",
                            export.name,
                            path.display()
                        );
                    }
                }
            }
        }

        Ok(())
    }

    /// Get the bind address for a specific port
    pub fn bind_addr_for(&self, port: u16) -> String {
        format!("{}:{}", self.server.bind_address, port)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn local_export(name: &str, uid: u32, path: &str) -> ExportConfig {
        ExportConfig {
            name: name.to_string(),
            uid,
            read_only: false,
            backend: BackendConfig::Local {
                path: PathBuf::from(path),
            },
        }
    }

    #[test]
    fn test_server_config_default() {
        let config = ServerConfig::default();
        assert_eq!(config.bind_address, "0.0.0.0");
        assert_eq!(config.nfs_port, 2049);
        assert_eq!(config.mount_port, 0);
    }

    #[test]
    fn test_logging_config_default() {
        let config = LoggingConfig::default();
        assert!(config.level.is_none());
    }

    #[test]
    fn test_config_default_has_one_export() {
        let config = Config::default();
        assert_eq!(config.server.bind_address, "0.0.0.0");
        assert_eq!(config.server.nfs_port, 2049);
        assert_eq!(config.server.mount_port, 0);
        assert!(config.logging.level.is_none());

        assert_eq!(config.exports.len(), 1);
        let export = &config.exports[0];
        assert_eq!(export.name, "/");
        assert_eq!(export.uid, 1);
        assert!(!export.read_only);
        match &export.backend {
            BackendConfig::Local { path } => {
                assert_eq!(path, &PathBuf::from("/tmp/nfs_exports"));
            }
        }
    }

    #[test]
    fn test_config_default_passes_validation() {
        Config::default()
            .validate()
            .expect("default config must be valid");
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
    fn test_parse_multiple_exports() {
        let toml = r#"
            [server]
            bind_address = "192.168.1.100"
            nfs_port = 2049
            mount_port = 20048

            [[exports]]
            name = "/data"
            uid = 1
            backend = "local"
            path = "/srv/data"

            [[exports]]
            name = "/backup"
            uid = 2
            read_only = true
            backend = "local"
            path = "/srv/backup"

            [logging]
            level = "trace"
        "#;

        let config: Config = toml::from_str(toml).expect("Failed to parse TOML");
        config.validate().expect("validation should pass");

        assert_eq!(config.server.bind_address, "192.168.1.100");
        assert_eq!(config.server.nfs_port, 2049);
        assert_eq!(config.server.mount_port, 20048);
        assert_eq!(config.logging.level, Some("trace".to_string()));

        assert_eq!(config.exports.len(), 2);

        assert_eq!(config.exports[0].name, "/data");
        assert_eq!(config.exports[0].uid, 1);
        assert!(!config.exports[0].read_only);
        match &config.exports[0].backend {
            BackendConfig::Local { path } => assert_eq!(path, &PathBuf::from("/srv/data")),
        }

        assert_eq!(config.exports[1].name, "/backup");
        assert_eq!(config.exports[1].uid, 2);
        assert!(config.exports[1].read_only);
        match &config.exports[1].backend {
            BackendConfig::Local { path } => assert_eq!(path, &PathBuf::from("/srv/backup")),
        }
    }

    #[test]
    fn test_parse_local_backend_tagged_enum() {
        // BackendConfig is a tagged enum flattened into ExportConfig — the
        // `backend = "local"` discriminator selects the Local variant.
        let toml = r#"
            [[exports]]
            name = "/exp"
            uid = 7
            backend = "local"
            path = "/var/exp"
        "#;

        let config: Config = toml::from_str(toml).expect("Failed to parse TOML");
        config.validate().expect("validation should pass");
        assert_eq!(config.exports.len(), 1);
        match &config.exports[0].backend {
            BackendConfig::Local { path } => assert_eq!(path, &PathBuf::from("/var/exp")),
        }
    }

    #[test]
    fn test_parse_unknown_backend_discriminator_is_rejected() {
        // The `backend` tag selects a BackendConfig variant; unknown values
        // (e.g. an S3 backend that doesn't exist yet) must fail at parse time
        // rather than silently fall through to a default.
        let toml = r#"
            [[exports]]
            name = "/data"
            uid = 1
            backend = "s3"
            bucket = "example"
        "#;
        let result: Result<Config, _> = toml::from_str(toml);
        assert!(
            result.is_err(),
            "unknown backend discriminator must fail to parse"
        );
    }

    #[test]
    fn test_empty_toml_uses_defaults_and_validates() {
        // An empty config file should round-trip through Config::default(),
        // pass validation, and yield exactly one export.
        let config: Config = toml::from_str("").expect("empty TOML must parse via defaults");
        config.validate().expect("default config must validate");
        assert_eq!(config.exports.len(), 1);
    }

    #[test]
    fn test_validate_rejects_empty_exports() {
        let config = Config {
            server: ServerConfig::default(),
            exports: vec![],
            logging: LoggingConfig::default(),
            admin: AdminConfig::default(),
        };
        let err = config.validate().expect_err("empty exports must fail");
        let msg = err.to_string();
        assert!(msg.contains("at least one"), "unexpected error: {msg}");
    }

    #[test]
    fn test_validate_rejects_duplicate_uid() {
        let config = Config {
            server: ServerConfig::default(),
            exports: vec![
                local_export("/a", 5, "/srv/a"),
                local_export("/b", 5, "/srv/b"),
            ],
            logging: LoggingConfig::default(),
            admin: AdminConfig::default(),
        };
        let err = config.validate().expect_err("duplicate uid must fail");
        let msg = err.to_string();
        assert!(
            msg.contains("Duplicate export uid"),
            "unexpected error: {msg}"
        );
        assert!(msg.contains("'/a'"), "should report first name; got: {msg}");
        assert!(
            msg.contains("'/b'"),
            "should report conflicting name; got: {msg}"
        );
    }

    #[test]
    fn test_validate_rejects_duplicate_name() {
        let config = Config {
            server: ServerConfig::default(),
            exports: vec![
                local_export("/data", 1, "/srv/a"),
                local_export("/data", 2, "/srv/b"),
            ],
            logging: LoggingConfig::default(),
            admin: AdminConfig::default(),
        };
        let err = config.validate().expect_err("duplicate name must fail");
        let msg = err.to_string();
        assert!(
            msg.contains("Duplicate export name"),
            "unexpected error: {msg}"
        );
    }

    #[test]
    fn test_validate_rejects_uid_zero() {
        let config = Config {
            server: ServerConfig::default(),
            exports: vec![local_export("/data", 0, "/srv/a")],
            logging: LoggingConfig::default(),
            admin: AdminConfig::default(),
        };
        let err = config.validate().expect_err("uid 0 must fail");
        let msg = err.to_string();
        assert!(msg.contains("uid 0"), "unexpected error: {msg}");
    }

    #[test]
    fn test_validate_rejects_name_without_leading_slash() {
        let config = Config {
            server: ServerConfig::default(),
            exports: vec![local_export("data", 1, "/srv/a")],
            logging: LoggingConfig::default(),
            admin: AdminConfig::default(),
        };
        let err = config.validate().expect_err("relative name must fail");
        let msg = err.to_string();
        assert!(
            msg.contains("must start with '/'"),
            "unexpected error: {msg}"
        );
    }

    #[test]
    fn test_validate_rejects_relative_local_path() {
        let mut config = Config::default();
        config.exports = vec![ExportConfig {
            name: "/data".to_string(),
            uid: 1,
            read_only: false,
            backend: BackendConfig::Local {
                path: PathBuf::from("relative/path"),
            },
        }];
        let err = config.validate().unwrap_err().to_string();
        assert!(err.contains("must be absolute"), "got: {}", err);
    }

    #[test]
    fn test_validate_accepts_multiple_distinct_exports() {
        let config = Config {
            server: ServerConfig::default(),
            exports: vec![
                local_export("/a", 1, "/srv/a"),
                local_export("/b", 2, "/srv/b"),
                local_export("/c", 3, "/srv/c"),
            ],
            logging: LoggingConfig::default(),
            admin: AdminConfig::default(),
        };
        config.validate().expect("distinct exports must validate");
    }

    /// Runs the same deserialization pipeline as `Config::load()` against an
    /// in-memory TOML string: `toml::Deserializer` wrapped in `serde_ignored`,
    /// with unknown keys promoted to an error. Lets tests exercise the full
    /// load path without needing an on-disk file.
    fn load_from_str(toml: &str) -> anyhow::Result<Config> {
        let mut unknown_keys: Vec<String> = Vec::new();
        let de = toml::Deserializer::new(toml);
        let config: Config = serde_ignored::deserialize(de, |path| {
            unknown_keys.push(path.to_string());
        })?;
        if !unknown_keys.is_empty() {
            anyhow::bail!("Unknown configuration key(s): {}", unknown_keys.join(", "));
        }
        Ok(config)
    }

    #[test]
    fn test_load_rejects_typo_in_export_level_field() {
        // Inside `[[exports]]`, `ExportConfig` cannot carry
        // `#[serde(deny_unknown_fields)]` (it flattens `BackendConfig`). For
        // typos like `readOnly` the cascade still rejects them: serde routes
        // unknown-to-`ExportConfig` keys through the flatten buffer into
        // `BackendConfig`, whose own `deny_unknown_fields` errors out. This
        // test pins that end-to-end behavior — regardless of which layer
        // produces the error, the load path must fail fast.
        let toml = r#"
[[exports]]
name = "/data"
uid = 1
backend = "local"
path = "/srv/data"
readOnly = true   # typo: should be read_only
"#;
        let err = load_from_str(toml).expect_err("readOnly typo must be rejected");
        let msg = err.to_string();
        assert!(
            msg.contains("readOnly"),
            "error should mention the offending key; got: {msg}"
        );
    }

    #[test]
    fn test_load_rejects_typo_in_server_section() {
        // `ServerConfig` does not (and cannot conveniently) carry
        // `deny_unknown_fields` because it uses `#[serde(default)]` to allow
        // partial overrides. Typos there used to be silently dropped; the
        // `serde_ignored` wrapper in `Config::load()` is what catches them.
        let toml = r#"
[server]
bind_addres = "127.0.0.1"   # typo: should be bind_address
nfs_port = 2049

[[exports]]
name = "/data"
uid = 1
backend = "local"
path = "/srv/data"
"#;
        let err = load_from_str(toml).expect_err("server-section typo must be rejected");
        let msg = err.to_string();
        assert!(
            msg.contains("bind_addres"),
            "error should mention the offending key; got: {msg}"
        );
    }

    #[test]
    fn test_parse_unknown_field_in_backend_is_rejected() {
        // An unknown field at the backend level (e.g. a typo of `path`) should
        // fail to parse rather than be silently dropped.
        let toml = r#"
            [[exports]]
            name = "/data"
            uid = 1
            backend = "local"
            pat = "/srv/data"
        "#;
        let result: Result<Config, _> = toml::from_str(toml);
        assert!(result.is_err(), "typo'd backend field should fail to parse");
    }

    #[test]
    fn test_parse_invalid_toml() {
        let result: Result<Config, _> = toml::from_str("this is not valid toml [[[");
        assert!(result.is_err());
    }

    #[test]
    fn test_legacy_fsal_section_is_rejected() {
        let toml = r#"
            [fsal]
            backend = "local"
            export_path = "/data"
        "#;
        let result: Result<Config, _> = toml::from_str(toml);
        assert!(
            result.is_err(),
            "legacy [fsal] section should fail to parse"
        );
        let err = result.unwrap_err().to_string();
        assert!(
            err.contains("unknown") || err.contains("fsal"),
            "error should mention unknown field; got: {}",
            err
        );
    }

    #[test]
    fn test_admin_config_defaults_when_section_absent() {
        // The defining contract for #25 phase 1: a config file that doesn't
        // mention `[admin]` at all must still parse and produce an inert
        // (`enabled = false`) admin section. Existing deployments must not
        // need to touch their TOML to keep building.
        let toml = r#"
            [[exports]]
            name = "/data"
            uid = 1
            backend = "local"
            path = "/srv/data"
        "#;
        let config: Config = toml::from_str(toml).expect("should parse without [admin]");
        assert!(!config.admin.enabled);
        assert_eq!(
            config.admin.socket_path,
            PathBuf::from("/run/arcticwolf/admin.sock")
        );
        assert_eq!(config.admin.socket_mode, 0o600);
    }

    #[test]
    fn test_admin_config_parses_explicit_values() {
        // Operators write the mode either as a decimal number or a Rust-style
        // octal literal; both must reach `AdminConfig::socket_mode` as the
        // same `u32`. We exercise the octal form here — TOML accepts `0o600`
        // as a numeric literal, but the assertion is on the decoded value.
        let toml = r#"
            [admin]
            enabled = true
            socket_path = "/tmp/aw-admin.sock"
            socket_mode = 0o640

            [[exports]]
            name = "/data"
            uid = 1
            backend = "local"
            path = "/srv/data"
        "#;
        let config: Config = toml::from_str(toml).expect("should parse [admin]");
        assert!(config.admin.enabled);
        assert_eq!(
            config.admin.socket_path,
            PathBuf::from("/tmp/aw-admin.sock")
        );
        assert_eq!(config.admin.socket_mode, 0o640);
    }

    #[test]
    fn test_admin_socket_mode_decimal_value() {
        // TOML has no octal syntax outside the Rust-style `0o...` literal, so
        // operators may write the mode as plain decimal. `384 == 0o600` must
        // round-trip through deserialization to the same numeric value as
        // `0o600` would. Pairs with `test_admin_config_parses_explicit_values`
        // (which exercises the `0o640` form).
        let toml = r#"
            [admin]
            enabled = true
            socket_path = "/tmp/aw-admin.sock"
            socket_mode = 384

            [[exports]]
            name = "/data"
            uid = 1
            backend = "local"
            path = "/srv/data"
        "#;
        let config: Config = toml::from_str(toml).expect("should parse decimal socket_mode");
        assert_eq!(config.admin.socket_mode, 0o600);
        config
            .validate()
            .expect("decimal socket_mode within range must validate");
    }

    #[test]
    fn test_admin_socket_mode_too_high_is_rejected() {
        // `chmod(2)` silently masks anything wider than 0o777, so passing
        // a 4-digit octal (or a very large decimal) would silently drop
        // bits without warning the operator. `validate()` must reject it.
        let mut config = Config::default();
        config.admin.socket_mode = 0o7777;
        let err = config
            .validate()
            .expect_err("socket_mode > 0o777 must be rejected")
            .to_string();
        assert!(
            err.contains("must be <= 0o777"),
            "error should explain the upper bound; got: {err}",
        );

        // Decimal far outside the chmod range — same code path, just confirms
        // we don't have a magic-octal shortcut bypass.
        let mut config = Config::default();
        config.admin.socket_mode = 9_999_999;
        let err = config
            .validate()
            .expect_err("absurd decimal socket_mode must be rejected")
            .to_string();
        assert!(
            err.contains("must be <= 0o777"),
            "error should explain the upper bound; got: {err}",
        );
    }

    #[test]
    fn test_admin_config_rejects_unknown_field() {
        // `AdminConfig` carries `deny_unknown_fields`, so a typo such as
        // `socket_perms` instead of `socket_mode` must fail at load time
        // rather than be silently dropped.
        let toml = r#"
            [admin]
            enabled = true
            socket_perms = 384

            [[exports]]
            name = "/data"
            uid = 1
            backend = "local"
            path = "/srv/data"
        "#;
        let result: Result<Config, _> = toml::from_str(toml);
        assert!(
            result.is_err(),
            "unknown field inside [admin] must fail to parse"
        );
    }
}
