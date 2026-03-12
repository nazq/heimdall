//! Configuration loading from TOML files.
//!
//! Heimdall looks for config in this order:
//! 1. `--config <path>` CLI flag
//! 2. `./heimdall.toml` in the current working directory
//! 3. `~/.config/heimdall/heimdall.toml`
//! 4. Built-in defaults

use serde::Deserialize;
use std::path::{Path, PathBuf};

/// Root configuration.
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct Config {
    /// Directory for socket and PID files.
    pub socket_dir: PathBuf,
    /// Scrollback buffer size in bytes.
    pub scrollback_bytes: usize,
    /// Environment variable name set on child processes with the session ID.
    pub session_env_var: String,
    /// State classifier to use.
    pub classifier: ClassifierConfig,
    /// Idle detection threshold in milliseconds.
    pub idle_threshold_ms: u64,
    /// State debounce period in milliseconds.
    pub debounce_ms: u64,
    /// Whether to signal the entire process group on kill/shutdown.
    ///
    /// When `true` (the default), SIGTERM/SIGKILL are sent to the process
    /// group (negative PID), ensuring grandchild processes are also terminated.
    /// Set to `false` to signal only the direct child, letting it manage its
    /// own descendants.
    pub kill_process_group: bool,
    /// Extra environment variables to inject into the child process.
    pub env: Vec<EnvVar>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            socket_dir: default_socket_dir(),
            scrollback_bytes: 64 * 1024,
            session_env_var: "HEIMDALL_SESSION_ID".into(),
            classifier: ClassifierConfig::default(),
            idle_threshold_ms: 3000,
            debounce_ms: 200,
            kill_process_group: true,
            env: Vec::new(),
        }
    }
}

/// State classifier selection.
#[derive(Debug, Clone, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum ClassifierConfig {
    /// Full state machine: idle, thinking, streaming, tool_use.
    /// Tuned for Claude Code's output patterns.
    #[default]
    Claude,
    /// Simple binary: idle or active.
    Simple,
    /// No state classification — always reports idle.
    None,
}

/// An extra environment variable to inject into the child.
#[derive(Debug, Clone, Deserialize)]
pub struct EnvVar {
    pub name: String,
    pub value: String,
}

fn default_socket_dir() -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".into());
    PathBuf::from(home).join(".local/share/heimdall/sessions")
}

const CONFIG_FILENAME: &str = "heimdall.toml";

/// Resolve config file path: CWD first, then ~/.config/heimdall/.
fn resolve_config_path() -> Option<PathBuf> {
    // Check current working directory first.
    let local = PathBuf::from(CONFIG_FILENAME);
    if local.exists() {
        return Some(local);
    }

    // Fall back to ~/.config/heimdall/heimdall.toml.
    let home = std::env::var("HOME").ok()?;
    let global = PathBuf::from(home)
        .join(".config/heimdall")
        .join(CONFIG_FILENAME);
    if global.exists() {
        return Some(global);
    }

    None
}

/// Load config from a TOML file, falling back to defaults.
///
/// If `path` is `Some` and the file doesn't exist, returns an error.
/// If `path` is `None`, attempts auto-resolution (CWD then ~/.config/heimdall/).
/// Falls back to defaults if no config file is found.
pub fn load(path: Option<&Path>) -> anyhow::Result<Config> {
    let resolved = match path {
        Some(p) => {
            if p.exists() {
                Some(p.to_path_buf())
            } else {
                anyhow::bail!("config file not found: {}", p.display());
            }
        }
        None => resolve_config_path(),
    };

    match resolved {
        Some(p) => {
            let contents = std::fs::read_to_string(&p)?;
            let config: Config = toml::from_str(&contents)?;
            Ok(config)
        }
        None => Ok(Config::default()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use std::sync::Mutex;

    /// Tests that mutate process-global state (CWD, HOME) must hold this lock
    /// to prevent flakes under parallel test execution.
    static GLOBAL_STATE: Mutex<()> = Mutex::new(());

    /// Issue #3: --config with a nonexistent path errors.
    #[test]
    fn load_nonexistent_explicit_path_errors() {
        let result = load(Some(Path::new("/tmp/does_not_exist_heimdall_test.toml")));
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(
            err.contains("not found"),
            "error should mention 'not found': {err}"
        );
    }

    /// Issue #3: when no config file exists, defaults are returned.
    #[test]
    fn load_none_returns_defaults() {
        let _lock = GLOBAL_STATE.lock().unwrap();
        let tmp = tempfile::tempdir().unwrap();
        let original = std::env::current_dir().unwrap();
        let original_home = std::env::var("HOME").ok();

        std::env::set_current_dir(tmp.path()).unwrap();
        unsafe { std::env::set_var("HOME", tmp.path()) };

        let config = load(None).unwrap();
        assert_eq!(config.scrollback_bytes, 64 * 1024);
        assert_eq!(config.session_env_var, "HEIMDALL_SESSION_ID");
        assert_eq!(config.idle_threshold_ms, 3000);
        assert_eq!(config.debounce_ms, 200);
        assert!(config.env.is_empty());

        std::env::set_current_dir(original).unwrap();
        if let Some(home) = original_home {
            unsafe { std::env::set_var("HOME", home) };
        }
    }

    /// Issue #3: CWD config is found first.
    #[test]
    fn load_cwd_config_found_first() {
        let _lock = GLOBAL_STATE.lock().unwrap();
        let tmp = tempfile::tempdir().unwrap();
        let config_path = tmp.path().join("heimdall.toml");
        let mut f = std::fs::File::create(&config_path).unwrap();
        writeln!(f, "scrollback_bytes = 1234").unwrap();

        let original = std::env::current_dir().unwrap();
        std::env::set_current_dir(tmp.path()).unwrap();

        let config = load(None).unwrap();
        assert_eq!(config.scrollback_bytes, 1234);

        std::env::set_current_dir(original).unwrap();
    }

    /// Issue #4: classifier = "claude" deserializes correctly.
    #[test]
    fn deserialize_classifier_claude() {
        let toml_str = r#"classifier = "claude""#;
        let config: Config = toml::from_str(toml_str).unwrap();
        assert!(matches!(config.classifier, ClassifierConfig::Claude));
    }

    /// Issue #4: classifier = "simple" deserializes correctly.
    #[test]
    fn deserialize_classifier_simple() {
        let toml_str = r#"classifier = "simple""#;
        let config: Config = toml::from_str(toml_str).unwrap();
        assert!(matches!(config.classifier, ClassifierConfig::Simple));
    }

    /// Issue #4: classifier = "none" deserializes correctly.
    #[test]
    fn deserialize_classifier_none() {
        let toml_str = r#"classifier = "none""#;
        let config: Config = toml::from_str(toml_str).unwrap();
        assert!(matches!(config.classifier, ClassifierConfig::None));
    }

    /// Issue #4: default classifier is Claude.
    #[test]
    fn default_classifier_is_claude() {
        let config = Config::default();
        assert!(matches!(config.classifier, ClassifierConfig::Claude));
    }

    /// Issue #4: env var injection config deserializes.
    #[test]
    fn deserialize_env_vars() {
        let toml_str = r#"
[[env]]
name = "MY_KEY"
value = "secret"

[[env]]
name = "RUST_LOG"
value = "debug"
"#;
        let config: Config = toml::from_str(toml_str).unwrap();
        assert_eq!(config.env.len(), 2);
        assert_eq!(config.env[0].name, "MY_KEY");
        assert_eq!(config.env[0].value, "secret");
        assert_eq!(config.env[1].name, "RUST_LOG");
        assert_eq!(config.env[1].value, "debug");
    }

    /// Issue #4: all fields deserialize from a complete config.
    #[test]
    fn deserialize_full_config() {
        let toml_str = r#"
socket_dir = "/tmp/test"
scrollback_bytes = 4096
session_env_var = "MY_SESSION"
classifier = "simple"
idle_threshold_ms = 5000
debounce_ms = 100

[[env]]
name = "FOO"
value = "bar"
"#;
        let config: Config = toml::from_str(toml_str).unwrap();
        assert_eq!(config.socket_dir, PathBuf::from("/tmp/test"));
        assert_eq!(config.scrollback_bytes, 4096);
        assert_eq!(config.session_env_var, "MY_SESSION");
        assert!(matches!(config.classifier, ClassifierConfig::Simple));
        assert_eq!(config.idle_threshold_ms, 5000);
        assert_eq!(config.debounce_ms, 100);
        assert_eq!(config.env.len(), 1);
    }

    /// Invalid classifier value produces an error.
    #[test]
    fn invalid_classifier_errors() {
        let toml_str = r#"classifier = "bogus""#;
        let result: Result<Config, _> = toml::from_str(toml_str);
        assert!(result.is_err());
    }

    /// Empty TOML string produces defaults.
    #[test]
    fn empty_toml_is_defaults() {
        let config: Config = toml::from_str("").unwrap();
        assert_eq!(config.scrollback_bytes, 64 * 1024);
        assert!(matches!(config.classifier, ClassifierConfig::Claude));
        assert!(config.env.is_empty());
    }

    /// Corrupt TOML produces a parse error.
    #[test]
    fn corrupt_toml_errors() {
        let result: Result<Config, _> = toml::from_str("not = [valid toml");
        assert!(result.is_err());
    }

    /// Partial config: only one field set, rest defaults.
    #[test]
    fn partial_config_fills_defaults() {
        let config: Config = toml::from_str("debounce_ms = 999").unwrap();
        assert_eq!(config.debounce_ms, 999);
        assert_eq!(config.idle_threshold_ms, 3000); // default
        assert_eq!(config.scrollback_bytes, 64 * 1024); // default
        assert!(matches!(config.classifier, ClassifierConfig::Claude)); // default
    }

    /// Zero scrollback_bytes is valid.
    #[test]
    fn zero_scrollback_is_valid() {
        let config: Config = toml::from_str("scrollback_bytes = 0").unwrap();
        assert_eq!(config.scrollback_bytes, 0);
    }

    /// Classifier case sensitivity — uppercase should fail.
    #[test]
    fn classifier_case_sensitive() {
        let result: Result<Config, _> = toml::from_str(r#"classifier = "Claude""#);
        assert!(result.is_err(), "classifier should be lowercase only");
    }

    /// load() with an explicit path that exists works.
    #[test]
    fn load_explicit_path_works() {
        let tmp = tempfile::tempdir().unwrap();
        let config_path = tmp.path().join("custom.toml");
        std::fs::write(&config_path, "idle_threshold_ms = 7777").unwrap();
        let config = load(Some(&config_path)).unwrap();
        assert_eq!(config.idle_threshold_ms, 7777);
    }

    /// kill_process_group defaults to true.
    #[test]
    fn kill_process_group_defaults_true() {
        let config: Config = toml::from_str("").unwrap();
        assert!(config.kill_process_group);
    }

    /// kill_process_group can be set to false.
    #[test]
    fn kill_process_group_can_be_disabled() {
        let config: Config = toml::from_str("kill_process_group = false").unwrap();
        assert!(!config.kill_process_group);
    }

    /// Negative numeric values in TOML are rejected for unsigned fields.
    #[test]
    fn negative_scrollback_errors() {
        let result: Result<Config, _> = toml::from_str("scrollback_bytes = -1");
        assert!(result.is_err());
    }
}
