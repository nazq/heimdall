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
    pub fn to_detach_args(&self) -> Vec<String> {
        let mut args = vec![
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
        args
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
