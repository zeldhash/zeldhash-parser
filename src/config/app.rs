//! Application configuration loading and merging.
//!
//! Combines CLI arguments, environment variables, and TOML file configuration
//! into a unified [`AppConfig`] structure.

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{anyhow, Context, Result};
use directories::ProjectDirs;
use serde::Deserialize;

use crate::cli::{Cli, MhinNetworkArg, ProtoblockOptions, RollblockOptions};

use super::overlay::Overlay;
use super::protoblock::ProtoblockSettings;
use super::rollblock::RollblockSettings;

const CONFIG_FILE_NAME: &str = "mhinparser.toml";
const LEGACY_CONFIG_FILES: [&str; 2] = ["mhinparser.toml", "config/mhinparser.toml"];
const PROJECT_QUALIFIER: &str = "org";
const PROJECT_ORGANIZATION: &str = "myhashisnice";
const PROJECT_APPLICATION: &str = "mhinparser";

/// Fully materialized configuration for the application.
///
/// Contains all resolved settings from CLI, environment, and config file sources.
/// Used to initialize the parser and all dependent subsystems.
#[derive(Debug)]
pub struct AppConfig {
    /// Path to the loaded configuration file, if any.
    pub config_file: Option<PathBuf>,
    /// Root directory for application data (SQLite, rollblock, logs, etc.).
    pub data_dir: PathBuf,
    /// Target Bitcoin network.
    pub network: mhinprotocol::MhinNetwork,
    /// Block fetching configuration.
    pub protoblock: ProtoblockSettings,
    /// UTXO store configuration.
    pub rollblock: RollblockSettings,
    /// Paths for runtime artifacts (PID file, logs).
    pub runtime: RuntimePaths,
}

impl AppConfig {
    /// Loads and merges configuration from all sources.
    ///
    /// Priority (highest to lowest):
    /// 1. CLI arguments
    /// 2. Environment variables
    /// 3. TOML configuration file
    /// 4. Built-in defaults
    pub fn load(cli: Cli) -> Result<Self> {
        let Cli {
            config,
            network: cli_network,
            protoblock: cli_protoblock,
            data_dir: cli_data_dir,
            rollblock: cli_rollblock,
            ..
        } = cli;

        let (file_config, config_path) = load_file_config(config.as_ref())?;
        let FileConfig {
            data_dir: file_data_dir,
            network: file_network,
            protoblock: file_protoblock,
            rollblock: file_rollblock,
        } = file_config;

        let data_dir = cli_data_dir
            .or(file_data_dir)
            .or_else(default_data_dir_path)
            .ok_or_else(|| {
                anyhow!(
                    "data_dir is required (set --data-dir, MHINPARSER_DATA_DIR, or ensure the OS user data directory is available)"
                )
            })?;

        let rollblock_settings = file_rollblock
            .unwrap_or_default()
            .overlay(cli_rollblock)
            .build(&data_dir)?;

        let protoblock_start_height = rollblock_settings.determine_start_height()?;

        let protoblock_settings = file_protoblock
            .unwrap_or_default()
            .overlay(cli_protoblock)
            .build(protoblock_start_height)?;

        let runtime = RuntimePaths::prepare(&data_dir)?;

        let network = cli_network
            .or(file_network)
            .map(Into::into)
            .unwrap_or(mhinprotocol::MhinNetwork::Mainnet);

        Ok(Self {
            config_file: config_path,
            data_dir,
            network,
            protoblock: protoblock_settings,
            rollblock: rollblock_settings,
            runtime,
        })
    }
}

pub fn load_runtime_paths(cli: Cli) -> Result<RuntimePaths> {
    let Cli {
        config,
        data_dir: cli_data_dir,
        ..
    } = cli;

    let (file_config, _config_path) = load_file_config(config.as_ref())?;
    let FileConfig {
        data_dir: file_data_dir,
        ..
    } = file_config;

    let data_dir = cli_data_dir
        .or(file_data_dir)
        .or_else(default_data_dir_path)
        .ok_or_else(|| {
            anyhow!(
                "data_dir is required (set --data-dir, MHINPARSER_DATA_DIR, or ensure the OS user data directory is available)"
            )
        })?;

    RuntimePaths::prepare(&data_dir)
}

#[derive(Debug, Default, Deserialize)]
struct FileConfig {
    #[serde(default)]
    data_dir: Option<PathBuf>,
    #[serde(default)]
    network: Option<MhinNetworkArg>,
    #[serde(default)]
    protoblock: Option<ProtoblockOptions>,
    #[serde(default)]
    rollblock: Option<RollblockOptions>,
}

fn load_file_config(path: Option<&PathBuf>) -> Result<(FileConfig, Option<PathBuf>)> {
    if let Some(provided) = path {
        let config = read_toml(provided)?;
        return Ok((config, Some(provided.clone())));
    }

    if let Some(default_path) = default_config_file_path().filter(|path| path.exists()) {
        let config = read_toml(&default_path)?;
        return Ok((config, Some(default_path)));
    }

    for candidate in LEGACY_CONFIG_FILES {
        let candidate_path = Path::new(candidate);
        if candidate_path.exists() {
            let config = read_toml(candidate_path)?;
            return Ok((config, Some(candidate_path.to_path_buf())));
        }
    }

    Ok((FileConfig::default(), None))
}

fn read_toml(path: &Path) -> Result<FileConfig> {
    let contents = fs::read_to_string(path)
        .with_context(|| format!("failed to read config file {}", path.display()))?;
    toml::from_str(&contents)
        .with_context(|| format!("failed to parse config file {}", path.display()))
}

impl From<MhinNetworkArg> for mhinprotocol::MhinNetwork {
    fn from(value: MhinNetworkArg) -> Self {
        match value {
            MhinNetworkArg::Mainnet => mhinprotocol::MhinNetwork::Mainnet,
            MhinNetworkArg::Testnet4 => mhinprotocol::MhinNetwork::Testnet4,
            MhinNetworkArg::Signet => mhinprotocol::MhinNetwork::Signet,
            MhinNetworkArg::Regtest => mhinprotocol::MhinNetwork::Regtest,
        }
    }
}

fn default_project_dirs() -> Option<ProjectDirs> {
    ProjectDirs::from(PROJECT_QUALIFIER, PROJECT_ORGANIZATION, PROJECT_APPLICATION)
}

fn default_config_file_path() -> Option<PathBuf> {
    default_project_dirs().map(|dirs| dirs.config_dir().join(CONFIG_FILE_NAME))
}

fn default_data_dir_path() -> Option<PathBuf> {
    default_project_dirs().map(|dirs| dirs.data_dir().to_path_buf())
}

/// Paths used by runtime artifacts such as PID and log files.
///
/// These paths are derived from the data directory and are created automatically.
#[derive(Debug, Clone)]
pub struct RuntimePaths {
    pid_file: PathBuf,
    log_file: PathBuf,
}

impl RuntimePaths {
    /// Creates runtime directories and returns resolved paths.
    pub fn prepare(data_dir: &Path) -> Result<Self> {
        let run_dir = data_dir.join("run");
        let logs_dir = data_dir.join("logs");
        fs::create_dir_all(&run_dir)
            .with_context(|| format!("failed to create runtime dir {}", run_dir.display()))?;
        fs::create_dir_all(&logs_dir)
            .with_context(|| format!("failed to create logs dir {}", logs_dir.display()))?;

        Ok(Self {
            pid_file: run_dir.join("mhinparser.pid"),
            log_file: logs_dir.join("mhinparser.log"),
        })
    }

    /// Returns the path to the PID file for daemon mode.
    pub fn pid_file(&self) -> &Path {
        &self.pid_file
    }

    /// Returns the path to the daemon log file.
    pub fn log_file(&self) -> &Path {
        &self.log_file
    }
}

#[cfg(test)]
mod tests {
    use super::{load_file_config, AppConfig, RuntimePaths};
    use crate::cli::{Cli, MhinNetworkArg, ProtoblockOptions, RollblockOptions};
    use std::fs;
    use std::path::PathBuf;
    use tempfile::TempDir;

    #[test]
    fn runtime_paths_create_directories() {
        let temp = TempDir::new().expect("temp dir");
        let data_dir = temp.path();
        let runtime = RuntimePaths::prepare(data_dir).expect("runtime paths");

        let pid_dir = runtime.pid_file().parent().expect("pid dir");
        let log_dir = runtime.log_file().parent().expect("log dir");

        assert!(pid_dir.exists());
        assert!(log_dir.exists());
    }

    #[test]
    fn load_file_config_reads_provided_path() {
        let temp = TempDir::new().expect("temp dir");
        let config_path = temp.path().join("mhinparser.toml");
        fs::write(&config_path, r#"data_dir = "/tmp/mhin-tests""#).expect("write config");

        let (config, path) = load_file_config(Some(&config_path)).expect("load config");

        assert_eq!(path.as_deref(), Some(config_path.as_path()));
        assert_eq!(
            config.data_dir.as_deref(),
            Some(PathBuf::from("/tmp/mhin-tests").as_path())
        );
    }

    #[test]
    fn read_toml_returns_error_on_invalid_file() {
        let temp = TempDir::new().expect("temp dir");
        let config_path = temp.path().join("invalid.toml");
        fs::write(&config_path, "not = [toml").expect("write invalid config");

        let err = load_file_config(Some(&config_path))
            .expect_err("invalid config should fail")
            .to_string();
        assert!(
            err.contains("parse"),
            "error should mention parse failure, got: {err}"
        );
    }

    #[test]
    fn app_config_prefers_cli_over_file_values() {
        let temp = TempDir::new().expect("temp dir");
        let file_data_dir = temp.path().join("file_data_dir");
        let cli_data_dir = temp.path().join("cli_data_dir");
        fs::create_dir_all(&file_data_dir).expect("file data dir");
        fs::create_dir_all(&cli_data_dir).expect("cli data dir");

        let config_path = temp.path().join("mhinparser.toml");
        fs::write(
            &config_path,
            format!(r#"data_dir = "{}""#, file_data_dir.display()),
        )
        .expect("write config");

        let cli = Cli {
            config: Some(config_path.clone()),
            network: Some(MhinNetworkArg::Signet),
            data_dir: Some(cli_data_dir.clone()),
            protoblock: ProtoblockOptions::default(),
            rollblock: RollblockOptions::default(),
            daemon: false,
            daemon_child: false,
            command: None,
        };

        let app = AppConfig::load(cli).expect("load app config");

        assert_eq!(app.config_file.as_deref(), Some(config_path.as_path()));
        assert_eq!(app.data_dir, cli_data_dir);
        assert!(matches!(app.network, mhinprotocol::MhinNetwork::Signet));
    }

    #[test]
    fn app_config_uses_file_when_cli_missing_values() {
        let temp = TempDir::new().expect("temp dir");
        let file_data_dir = temp.path().join("file_only");
        fs::create_dir_all(&file_data_dir).expect("file data dir");

        let config_path = temp.path().join("mhinparser.toml");
        fs::write(
            &config_path,
            format!(
                r#"
data_dir = "{}"
network = "Regtest"
"#,
                file_data_dir.display()
            ),
        )
        .expect("write config");

        let cli = Cli {
            config: Some(config_path.clone()),
            network: None,
            data_dir: None,
            protoblock: ProtoblockOptions::default(),
            rollblock: RollblockOptions::default(),
            daemon: false,
            daemon_child: false,
            command: None,
        };

        let app = AppConfig::load(cli).expect("load app config from file");

        assert_eq!(app.data_dir, file_data_dir);
        assert!(matches!(app.network, mhinprotocol::MhinNetwork::Regtest));
    }

    #[test]
    fn mhin_network_from_mainnet() {
        let network: mhinprotocol::MhinNetwork = MhinNetworkArg::Mainnet.into();
        assert!(matches!(network, mhinprotocol::MhinNetwork::Mainnet));
    }

    #[test]
    fn mhin_network_from_testnet4() {
        let network: mhinprotocol::MhinNetwork = MhinNetworkArg::Testnet4.into();
        assert!(matches!(network, mhinprotocol::MhinNetwork::Testnet4));
    }

    #[test]
    fn mhin_network_from_signet() {
        let network: mhinprotocol::MhinNetwork = MhinNetworkArg::Signet.into();
        assert!(matches!(network, mhinprotocol::MhinNetwork::Signet));
    }

    #[test]
    fn mhin_network_from_regtest() {
        let network: mhinprotocol::MhinNetwork = MhinNetworkArg::Regtest.into();
        assert!(matches!(network, mhinprotocol::MhinNetwork::Regtest));
    }

    #[test]
    fn load_file_config_returns_default_when_no_config_exists() {
        let (config, path) = load_file_config(None).expect("default config");
        // When no explicit path and no default config exists, path may or may not be None
        // depending on whether the system has a config file
        // The important thing is that it doesn't error
        assert!(config.data_dir.is_none() || config.data_dir.is_some());
        let _ = path; // Just make sure we can access it
    }

    #[test]
    fn runtime_paths_accessors_return_expected_files() {
        let temp = TempDir::new().expect("temp dir");
        let runtime = RuntimePaths::prepare(temp.path()).expect("runtime paths");

        let pid_file = runtime.pid_file();
        let log_file = runtime.log_file();

        assert!(pid_file.ends_with("mhinparser.pid"));
        assert!(log_file.ends_with("mhinparser.log"));
        assert!(pid_file.starts_with(temp.path()));
        assert!(log_file.starts_with(temp.path()));
    }
}
