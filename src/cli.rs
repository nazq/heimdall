//! CLI definition (clap structs) and config merge logic.

use crate::config;
use clap::{Parser, Subcommand};
use std::path::PathBuf;

#[derive(Parser)]
#[command(
    name = "hm",
    about = "PTY session supervisor",
    version = concat!(env!("CARGO_PKG_VERSION"), " (", env!("HM_BUILD_TIME"), ")")
)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Command,

    /// Path to config file.
    #[arg(long, global = true)]
    pub config: Option<PathBuf>,
}

#[derive(Subcommand)]
#[allow(clippy::large_enum_variant)] // CLI enum parsed once at startup
pub enum Command {
    /// Launch a supervised session and attach to it.
    Run {
        /// Session identifier (used for socket filename).
        #[arg(long)]
        id: String,
        /// Working directory for the child process.
        #[arg(long, default_value = ".")]
        workdir: PathBuf,
        /// Directory for socket and pid files (overrides config).
        #[arg(long)]
        socket_dir: Option<PathBuf>,
        /// Terminal columns.
        #[arg(long, default_value_t = 220)]
        cols: u16,
        /// Terminal rows.
        #[arg(long, default_value_t = 50)]
        rows: u16,
        /// Run the supervisor in the background without attaching.
        #[arg(long)]
        detach: bool,
        /// Log file path (overrides config).
        /// Defaults to <socket_dir>/<id>.log. Set to /dev/null to disable.
        #[arg(long)]
        log_file: Option<PathBuf>,
        /// Log level: trace, debug, info, warn, error (overrides config).
        #[arg(long)]
        log_level: Option<String>,
        /// Additional tracing filter directives for dependency crates.
        /// Uses EnvFilter syntax, e.g. "tokio=warn,nix=error".
        #[arg(long)]
        log_filter: Option<String>,
        /// Scrollback buffer size in bytes (overrides config).
        #[arg(long)]
        scrollback_bytes: Option<usize>,
        /// State classifier: simple, claude, or none (overrides config).
        /// When combined with --idle-threshold-ms / --debounce-ms, the
        /// classifier is created with those params; otherwise it uses
        /// the config file values or built-in defaults.
        #[arg(long)]
        classifier: Option<String>,
        /// Idle detection threshold in milliseconds (overrides classifier config).
        #[arg(long)]
        idle_threshold_ms: Option<u64>,
        /// State debounce period in milliseconds (overrides classifier config).
        /// Only meaningful for the claude classifier.
        #[arg(long)]
        debounce_ms: Option<u64>,
        /// Signal the process group on kill (overrides config).
        /// Use --kill-process-group or --no-kill-process-group.
        #[arg(long, action = clap::ArgAction::Set, num_args = 0..=1, default_missing_value = "true")]
        kill_process_group: Option<bool>,
        /// Environment variable name for the session ID (overrides config).
        #[arg(long)]
        session_env_var: Option<String>,
        /// Child command and arguments (everything after --).
        #[arg(trailing_var_arg = true, required = true)]
        cmd: Vec<String>,
    },
    /// Attach to a running session (terminal passthrough).
    Attach {
        /// Session identifier to attach to.
        id: String,
        /// Directory for socket files (overrides config).
        #[arg(long)]
        socket_dir: Option<PathBuf>,
    },
    /// Query status of a session.
    Status {
        /// Session identifier.
        id: String,
        /// Directory for socket files (overrides config).
        #[arg(long)]
        socket_dir: Option<PathBuf>,
    },
    /// List active sessions.
    #[command(name = "ls")]
    List {
        /// Directory for socket files (overrides config).
        #[arg(long)]
        socket_dir: Option<PathBuf>,
    },
    /// Kill a session (graceful shutdown).
    Kill {
        /// Session identifier.
        id: String,
        /// Directory for socket files (overrides config).
        #[arg(long)]
        socket_dir: Option<PathBuf>,
    },
    /// Remove orphaned log files from dead sessions.
    /// Dry-run by default — pass --force to actually delete.
    Clean {
        /// Directory for session files (overrides config).
        #[arg(long)]
        socket_dir: Option<PathBuf>,
        /// Keep logs modified within this duration (e.g. 24h, 7d, 1h).
        /// Default: 24h.
        #[arg(long, default_value = "24h")]
        older_than: String,
        /// Actually delete files (default is dry-run).
        #[arg(long)]
        force: bool,
    },
}

/// Bundled parameters for session launch.
pub struct SessionParams {
    pub id: String,
    pub workdir: PathBuf,
    pub socket_dir: PathBuf,
    pub cols: u16,
    pub rows: u16,
    pub cmd: Vec<String>,
    pub cfg: config::Config,
    pub log_file: PathBuf,
}

impl SessionParams {
    /// Produce CLI arguments for `hm run --detach` re-exec.
    ///
    /// Used by `launch_and_attach` to spawn the supervisor as a background
    /// process with the same resolved parameters.
    /// Write the resolved config to a temp file in `socket_dir` and produce
    /// CLI arguments for `hm run --detach` re-exec.
    ///
    /// This avoids the re-parse boundary: the detached supervisor reads the
    /// full resolved config (including `[[env]]`, classifier thresholds, etc.)
    /// instead of reconstructing it from CLI flags.
    pub fn to_detach_args(&self) -> std::io::Result<Vec<String>> {
        // Write resolved config to a persistent file the supervisor can read.
        // Lives in socket_dir alongside .sock/.pid/.log — cleaned up on exit
        // isn't critical since it's small and the session owns the directory.
        let config_path = self.socket_dir.join(format!("{}.config.toml", self.id));
        std::fs::create_dir_all(&self.socket_dir)?;
        std::fs::write(&config_path, self.cfg.to_toml())?;

        let mut args = vec![
            "--config".into(),
            config_path.to_string_lossy().into_owned(),
            "run".into(),
            "--id".into(),
            self.id.clone(),
            "--workdir".into(),
            self.workdir.to_string_lossy().into_owned(),
            "--socket-dir".into(),
            self.socket_dir.to_string_lossy().into_owned(),
            "--cols".into(),
            self.cols.to_string(),
            "--rows".into(),
            self.rows.to_string(),
            "--detach".into(),
            "--log-file".into(),
            self.log_file.to_string_lossy().into_owned(),
            "--".into(),
        ];
        args.extend(self.cmd.iter().cloned());
        Ok(args)
    }
}

/// Raw CLI arguments for the `run` subcommand before config merge.
pub struct RunArgs {
    pub id: String,
    pub workdir: PathBuf,
    pub socket_dir: Option<PathBuf>,
    pub cols: u16,
    pub rows: u16,
    pub log_file: Option<PathBuf>,
    pub log_level: Option<String>,
    pub log_filter: Option<String>,
    pub scrollback_bytes: Option<usize>,
    pub classifier: Option<String>,
    pub idle_threshold_ms: Option<u64>,
    pub debounce_ms: Option<u64>,
    pub kill_process_group: Option<bool>,
    pub session_env_var: Option<String>,
    pub cmd: Vec<String>,
}

/// Apply CLI overrides to a loaded config, returning `SessionParams`.
///
/// Classifier merge logic: `--classifier` switches the type (fresh defaults);
/// `--idle-threshold-ms` / `--debounce-ms` override per-classifier params.
pub fn merge_run_args(cfg: config::Config, args: RunArgs) -> anyhow::Result<SessionParams> {
    let RunArgs {
        id,
        workdir,
        socket_dir,
        cols,
        rows,
        log_file,
        log_level,
        log_filter,
        scrollback_bytes,
        classifier,
        idle_threshold_ms,
        debounce_ms,
        kill_process_group,
        session_env_var,
        cmd,
    } = args;
    let mut cfg = cfg;
    let dir = socket_dir.unwrap_or_else(|| cfg.socket_dir.clone());

    if let Some(v) = scrollback_bytes {
        cfg.scrollback_bytes = v;
    }
    if let Some(v) = kill_process_group {
        cfg.kill_process_group = v;
    }
    if let Some(v) = session_env_var {
        cfg.session_env_var = v;
    }
    if let Some(v) = log_level {
        cfg.log_level = v;
    }
    if let Some(v) = log_filter {
        cfg.log_filter = Some(v);
    }

    if let Some(v) = classifier {
        cfg.classifier = match v.as_str() {
            "simple" => config::ClassifierConfig::Simple {
                idle_threshold_ms: idle_threshold_ms.unwrap_or(config::DEFAULT_IDLE_THRESHOLD_MS),
            },
            "claude" => config::ClassifierConfig::Claude {
                idle_threshold_ms: idle_threshold_ms.unwrap_or(config::DEFAULT_IDLE_THRESHOLD_MS),
                debounce_ms: debounce_ms.unwrap_or(config::DEFAULT_DEBOUNCE_MS),
            },
            "none" => config::ClassifierConfig::None,
            other => {
                anyhow::bail!("unknown classifier: {other} (expected simple, claude, or none)")
            }
        };
    } else {
        cfg.classifier = match cfg.classifier {
            config::ClassifierConfig::Simple {
                idle_threshold_ms: existing,
            } => config::ClassifierConfig::Simple {
                idle_threshold_ms: idle_threshold_ms.unwrap_or(existing),
            },
            config::ClassifierConfig::Claude {
                idle_threshold_ms: existing_idle,
                debounce_ms: existing_debounce,
            } => config::ClassifierConfig::Claude {
                idle_threshold_ms: idle_threshold_ms.unwrap_or(existing_idle),
                debounce_ms: debounce_ms.unwrap_or(existing_debounce),
            },
            config::ClassifierConfig::None => config::ClassifierConfig::None,
        };
    }

    // log_file precedence: CLI > config > <socket_dir>/<id>.log
    let log = log_file
        .or(cfg.log_file.take())
        .unwrap_or_else(|| dir.join(format!("{id}.log")));

    Ok(SessionParams {
        id,
        workdir,
        socket_dir: dir,
        cols,
        rows,
        cmd,
        cfg,
        log_file: log,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a SessionParams with non-default config.
    fn make_params(classifier: config::ClassifierConfig) -> SessionParams {
        let tmp = tempfile::tempdir().unwrap();
        let socket_dir = tmp.path().to_path_buf();
        // Leak the TempDir so it lives past the test (config file is written there).
        std::mem::forget(tmp);
        let cfg = config::Config {
            classifier,
            log_level: "debug".into(),
            log_filter: Some("tokio=warn,nix=error".into()),
            scrollback_bytes: 128_000,
            kill_process_group: false,
            session_env_var: "MY_SESSION".into(),
            env: vec![config::EnvVar {
                name: "FOO".into(),
                value: "bar".into(),
            }],
            ..Default::default()
        };

        SessionParams {
            id: "test-sess".into(),
            workdir: PathBuf::from("/tmp/work"),
            socket_dir,
            cols: 200,
            rows: 40,
            cmd: vec!["bash".into(), "-c".into(), "echo hi".into()],
            cfg,
            log_file: PathBuf::from("/tmp/test.log"),
        }
    }

    #[test]
    fn to_detach_args_writes_config_file() {
        let p = make_params(config::ClassifierConfig::default());
        let args = p.to_detach_args().unwrap();

        // Should include --config pointing to a file in socket_dir.
        let idx = args.iter().position(|a| a == "--config").unwrap();
        let config_path = PathBuf::from(&args[idx + 1]);
        assert!(config_path.exists(), "config file should be written");

        // The file should be valid TOML that deserializes back.
        let contents = std::fs::read_to_string(&config_path).unwrap();
        let roundtrip: config::Config = toml::from_str(&contents).unwrap();
        assert_eq!(roundtrip.log_level, "debug");
        assert_eq!(
            roundtrip.log_filter.as_deref(),
            Some("tokio=warn,nix=error")
        );
        assert_eq!(roundtrip.scrollback_bytes, 128_000);
        assert!(!roundtrip.kill_process_group);
        assert_eq!(roundtrip.session_env_var, "MY_SESSION");

        // env vars round-trip.
        assert_eq!(roundtrip.env.len(), 1);
        assert_eq!(roundtrip.env[0].name, "FOO");
        assert_eq!(roundtrip.env[0].value, "bar");
    }

    #[test]
    fn to_detach_args_includes_detach_and_session_params() {
        let p = make_params(config::ClassifierConfig::default());
        let args = p.to_detach_args().unwrap();

        assert!(args.contains(&"--detach".to_string()));
        assert!(args.contains(&"--id".to_string()));
        assert!(args.contains(&"test-sess".to_string()));

        let sep = args.iter().position(|a| a == "--").unwrap();
        assert_eq!(&args[sep + 1..], &["bash", "-c", "echo hi"]);
    }

    #[test]
    fn to_detach_args_config_roundtrip_claude_classifier() {
        let p = make_params(config::ClassifierConfig::Claude {
            idle_threshold_ms: 5000,
            debounce_ms: 200,
        });
        let args = p.to_detach_args().unwrap();

        let idx = args.iter().position(|a| a == "--config").unwrap();
        let contents = std::fs::read_to_string(&args[idx + 1]).unwrap();
        let roundtrip: config::Config = toml::from_str(&contents).unwrap();

        assert_eq!(roundtrip.classifier.name(), "claude");
        assert_eq!(roundtrip.classifier.idle_threshold_ms(), 5000);
        assert_eq!(roundtrip.classifier.debounce_ms(), 200);
    }

    #[test]
    fn to_detach_args_config_roundtrip_none_classifier() {
        let p = make_params(config::ClassifierConfig::None);
        let args = p.to_detach_args().unwrap();

        let idx = args.iter().position(|a| a == "--config").unwrap();
        let contents = std::fs::read_to_string(&args[idx + 1]).unwrap();
        let roundtrip: config::Config = toml::from_str(&contents).unwrap();

        assert_eq!(roundtrip.classifier.name(), "none");
    }

    // ── merge_run_args tests ─────────────────────────────────────────

    #[test]
    fn merge_cli_overrides_config() {
        let cfg = config::Config::default();
        let args = RunArgs {
            id: "test".into(),
            workdir: PathBuf::from("."),
            socket_dir: None,
            cols: 80,
            rows: 24,
            log_file: None,
            log_level: Some("debug".into()),
            log_filter: Some("tokio=warn".into()),
            scrollback_bytes: Some(256_000),
            classifier: None,
            idle_threshold_ms: None,
            debounce_ms: None,
            kill_process_group: Some(false),
            session_env_var: Some("CUSTOM_VAR".into()),
            cmd: vec!["bash".into()],
        };
        let params = merge_run_args(cfg, args).unwrap();
        assert_eq!(params.cfg.log_level, "debug");
        assert_eq!(params.cfg.log_filter.as_deref(), Some("tokio=warn"));
        assert_eq!(params.cfg.scrollback_bytes, 256_000);
        assert!(!params.cfg.kill_process_group);
        assert_eq!(params.cfg.session_env_var, "CUSTOM_VAR");
    }

    #[test]
    fn merge_config_defaults_preserved() {
        let cfg = config::Config::default();
        let args = RunArgs {
            id: "test".into(),
            workdir: PathBuf::from("."),
            socket_dir: None,
            cols: 80,
            rows: 24,
            log_file: None,
            log_level: None,
            log_filter: None,
            scrollback_bytes: None,
            classifier: None,
            idle_threshold_ms: None,
            debounce_ms: None,
            kill_process_group: None,
            session_env_var: None,
            cmd: vec!["bash".into()],
        };
        let params = merge_run_args(cfg, args).unwrap();
        assert_eq!(params.cfg.log_level, config::DEFAULT_LOG_LEVEL);
        assert!(params.cfg.log_filter.is_none());
        assert_eq!(params.cfg.scrollback_bytes, 64 * 1024);
        assert!(params.cfg.kill_process_group);
        assert_eq!(params.cfg.session_env_var, "HEIMDALL_SESSION_ID");
    }

    #[test]
    fn merge_classifier_switch_resets_defaults() {
        let cfg = config::Config {
            classifier: config::ClassifierConfig::Simple {
                idle_threshold_ms: 9999,
            },
            ..Default::default()
        };
        let args = RunArgs {
            id: "test".into(),
            workdir: PathBuf::from("."),
            socket_dir: None,
            cols: 80,
            rows: 24,
            log_file: None,
            log_level: None,
            log_filter: None,
            scrollback_bytes: None,
            classifier: Some("claude".into()),
            idle_threshold_ms: None,
            debounce_ms: None,
            kill_process_group: None,
            session_env_var: None,
            cmd: vec!["bash".into()],
        };
        let params = merge_run_args(cfg, args).unwrap();
        // Switching classifier resets to that classifier's defaults.
        assert_eq!(params.cfg.classifier.name(), "claude");
        assert_eq!(
            params.cfg.classifier.idle_threshold_ms(),
            config::DEFAULT_IDLE_THRESHOLD_MS
        );
        assert_eq!(
            params.cfg.classifier.debounce_ms(),
            config::DEFAULT_DEBOUNCE_MS
        );
    }

    #[test]
    fn merge_classifier_switch_with_overrides() {
        let cfg = config::Config::default();
        let args = RunArgs {
            id: "test".into(),
            workdir: PathBuf::from("."),
            socket_dir: None,
            cols: 80,
            rows: 24,
            log_file: None,
            log_level: None,
            log_filter: None,
            scrollback_bytes: None,
            classifier: Some("claude".into()),
            idle_threshold_ms: Some(5000),
            debounce_ms: Some(200),
            kill_process_group: None,
            session_env_var: None,
            cmd: vec!["bash".into()],
        };
        let params = merge_run_args(cfg, args).unwrap();
        assert_eq!(params.cfg.classifier.idle_threshold_ms(), 5000);
        assert_eq!(params.cfg.classifier.debounce_ms(), 200);
    }

    #[test]
    fn merge_log_file_default_derived_from_socket_dir() {
        let cfg = config::Config::default();
        let args = RunArgs {
            id: "my-session".into(),
            workdir: PathBuf::from("."),
            socket_dir: Some(PathBuf::from("/tmp/socks")),
            cols: 80,
            rows: 24,
            log_file: None,
            log_level: None,
            log_filter: None,
            scrollback_bytes: None,
            classifier: None,
            idle_threshold_ms: None,
            debounce_ms: None,
            kill_process_group: None,
            session_env_var: None,
            cmd: vec!["bash".into()],
        };
        let params = merge_run_args(cfg, args).unwrap();
        assert_eq!(params.log_file, PathBuf::from("/tmp/socks/my-session.log"));
    }

    #[test]
    fn merge_unknown_classifier_errors() {
        let cfg = config::Config::default();
        let args = RunArgs {
            id: "test".into(),
            workdir: PathBuf::from("."),
            socket_dir: None,
            cols: 80,
            rows: 24,
            log_file: None,
            log_level: None,
            log_filter: None,
            scrollback_bytes: None,
            classifier: Some("bogus".into()),
            idle_threshold_ms: None,
            debounce_ms: None,
            kill_process_group: None,
            session_env_var: None,
            cmd: vec!["bash".into()],
        };
        assert!(merge_run_args(cfg, args).is_err());
    }
}
