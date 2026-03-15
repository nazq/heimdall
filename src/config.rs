//! Configuration loading from TOML files.
//!
//! Heimdall looks for config in this order:
//! 1. `--config <path>` CLI flag
//! 2. `./heimdall.toml` in the current working directory
//! 3. `~/.config/heimdall/heimdall.toml`
//! 4. Built-in defaults

use serde::Deserialize;
use std::path::{Path, PathBuf};

/// Default log level for the supervisor.
pub const DEFAULT_LOG_LEVEL: &str = "info";

/// Default detach key: Ctrl-\ (0x1C).
pub const DEFAULT_DETACH_KEY: u8 = 0x1C;

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
    /// State classifier to use, with per-classifier parameters.
    pub classifier: ClassifierConfig,
    /// Whether to signal the entire process group on kill/shutdown.
    ///
    /// When `true` (the default), SIGTERM/SIGKILL are sent to the process
    /// group (negative PID), ensuring grandchild processes are also terminated.
    /// Set to `false` to signal only the direct child, letting it manage its
    /// own descendants.
    pub kill_process_group: bool,
    /// Log file path. When not set, defaults to `<socket_dir>/<id>.log`.
    /// Set to `/dev/null` to disable logging.
    pub log_file: Option<PathBuf>,
    /// Log level for heimdall's own messages (trace, debug, info, warn, error).
    pub log_level: String,
    /// Additional tracing filter directives for dependency crates.
    /// Uses `tracing_subscriber::EnvFilter` syntax, e.g. "tokio=warn,nix=error".
    /// `RUST_LOG` env var takes precedence over both `log_level` and `log_filter`.
    pub log_filter: Option<String>,
    /// Detach key byte. When this byte appears in stdin input, the attach
    /// client disconnects and the session keeps running in the background.
    /// Default: `0x1C` (Ctrl-\). Set to `0` to disable.
    pub detach_key: u8,
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
            kill_process_group: true,
            log_file: None,
            log_level: DEFAULT_LOG_LEVEL.into(),
            log_filter: None,
            detach_key: DEFAULT_DETACH_KEY,
            env: Vec::new(),
        }
    }
}

/// State classifier selection with per-classifier parameters.
///
/// Supports two TOML representations:
///
/// **String shorthand** (all defaults for the classifier):
/// ```toml
/// classifier = "simple"
/// ```
///
/// **Table form** (custom parameters):
/// ```toml
/// [classifier.claude]
/// idle_threshold_ms = 5000
/// debounce_ms = 100
/// ```
#[derive(Debug, Clone, Deserialize)]
#[serde(try_from = "ClassifierRaw")]
pub enum ClassifierConfig {
    /// Full state machine: idle, thinking, streaming, tool_use.
    /// Tuned for Claude Code's output patterns.
    Claude {
        idle_threshold_ms: u64,
        debounce_ms: u64,
    },
    /// Simple binary: idle or active.
    Simple { idle_threshold_ms: u64 },
    /// No state classification — always reports idle.
    None,
}

impl Default for ClassifierConfig {
    fn default() -> Self {
        Self::Simple {
            idle_threshold_ms: DEFAULT_IDLE_THRESHOLD_MS,
        }
    }
}

impl ClassifierConfig {
    /// Idle detection threshold in milliseconds.
    pub fn idle_threshold_ms(&self) -> u64 {
        match self {
            Self::Claude {
                idle_threshold_ms, ..
            } => *idle_threshold_ms,
            Self::Simple { idle_threshold_ms } => *idle_threshold_ms,
            Self::None => 0,
        }
    }

    /// State debounce period in milliseconds (only meaningful for claude).
    pub fn debounce_ms(&self) -> u64 {
        match self {
            Self::Claude { debounce_ms, .. } => *debounce_ms,
            Self::Simple { .. } | Self::None => 0,
        }
    }

    /// The classifier type name as a string.
    pub fn name(&self) -> &'static str {
        match self {
            Self::Claude { .. } => "claude",
            Self::Simple { .. } => "simple",
            Self::None => "none",
        }
    }
}

/// Default idle detection threshold.
pub const DEFAULT_IDLE_THRESHOLD_MS: u64 = 3000;
/// Default debounce period.
pub const DEFAULT_DEBOUNCE_MS: u64 = 200;

// -- Serde intermediate types for flexible TOML parsing --

/// Raw deserialization target that accepts both string and table forms.
#[derive(Deserialize)]
#[serde(untagged)]
enum ClassifierRaw {
    /// `classifier = "simple"` — string shorthand with defaults.
    Name(String),
    /// `[classifier.claude]\nidle_threshold_ms = 5000` — table with one key.
    Table(ClassifierTable),
}

/// Table form: exactly one key (the classifier name) mapping to its params.
#[derive(Deserialize)]
struct ClassifierTable {
    #[serde(default)]
    claude: Option<ClaudeParams>,
    #[serde(default)]
    simple: Option<SimpleParams>,
    #[serde(default)]
    none: Option<NoneParams>,
}

#[derive(Deserialize)]
#[serde(default)]
struct ClaudeParams {
    idle_threshold_ms: u64,
    debounce_ms: u64,
}

impl Default for ClaudeParams {
    fn default() -> Self {
        Self {
            idle_threshold_ms: DEFAULT_IDLE_THRESHOLD_MS,
            debounce_ms: DEFAULT_DEBOUNCE_MS,
        }
    }
}

#[derive(Deserialize)]
#[serde(default)]
struct SimpleParams {
    idle_threshold_ms: u64,
}

impl Default for SimpleParams {
    fn default() -> Self {
        Self {
            idle_threshold_ms: DEFAULT_IDLE_THRESHOLD_MS,
        }
    }
}

/// None classifier has no parameters, but we accept an empty table.
#[derive(Deserialize, Default)]
#[serde(default)]
struct NoneParams {}

impl TryFrom<ClassifierRaw> for ClassifierConfig {
    type Error = String;

    fn try_from(raw: ClassifierRaw) -> Result<Self, Self::Error> {
        match raw {
            ClassifierRaw::Name(name) => match name.as_str() {
                "claude" => Ok(Self::Claude {
                    idle_threshold_ms: DEFAULT_IDLE_THRESHOLD_MS,
                    debounce_ms: DEFAULT_DEBOUNCE_MS,
                }),
                "simple" => Ok(Self::Simple {
                    idle_threshold_ms: DEFAULT_IDLE_THRESHOLD_MS,
                }),
                "none" => Ok(Self::None),
                other => Err(format!(
                    "unknown classifier: {other} (expected simple, claude, or none)"
                )),
            },
            ClassifierRaw::Table(table) => {
                let count = table.claude.is_some() as u8
                    + table.simple.is_some() as u8
                    + table.none.is_some() as u8;
                if count != 1 {
                    return Err(format!(
                        "classifier table must have exactly one key (claude, simple, or none), got {count}"
                    ));
                }
                if let Some(p) = table.claude {
                    Ok(Self::Claude {
                        idle_threshold_ms: p.idle_threshold_ms,
                        debounce_ms: p.debounce_ms,
                    })
                } else if let Some(p) = table.simple {
                    Ok(Self::Simple {
                        idle_threshold_ms: p.idle_threshold_ms,
                    })
                } else {
                    Ok(Self::None)
                }
            }
        }
    }
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
        assert_eq!(config.classifier.idle_threshold_ms(), 3000);
        assert_eq!(config.classifier.debounce_ms(), 0); // simple has no debounce
        assert!(config.log_file.is_none());
        assert_eq!(config.log_level, "info");
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

    // -- String shorthand tests --

    #[test]
    fn deserialize_classifier_claude_string() {
        let config: Config = toml::from_str(r#"classifier = "claude""#).unwrap();
        assert!(matches!(config.classifier, ClassifierConfig::Claude { .. }));
        assert_eq!(config.classifier.idle_threshold_ms(), 3000);
        assert_eq!(config.classifier.debounce_ms(), 200);
    }

    #[test]
    fn deserialize_classifier_simple_string() {
        let config: Config = toml::from_str(r#"classifier = "simple""#).unwrap();
        assert!(matches!(config.classifier, ClassifierConfig::Simple { .. }));
        assert_eq!(config.classifier.idle_threshold_ms(), 3000);
    }

    #[test]
    fn deserialize_classifier_none_string() {
        let config: Config = toml::from_str(r#"classifier = "none""#).unwrap();
        assert!(matches!(config.classifier, ClassifierConfig::None));
    }

    // -- Table form tests --

    #[test]
    fn deserialize_classifier_claude_table() {
        let toml_str = r#"
[classifier.claude]
idle_threshold_ms = 5000
debounce_ms = 100
"#;
        let config: Config = toml::from_str(toml_str).unwrap();
        assert!(matches!(config.classifier, ClassifierConfig::Claude { .. }));
        assert_eq!(config.classifier.idle_threshold_ms(), 5000);
        assert_eq!(config.classifier.debounce_ms(), 100);
    }

    #[test]
    fn deserialize_classifier_simple_table() {
        let toml_str = r#"
[classifier.simple]
idle_threshold_ms = 7000
"#;
        let config: Config = toml::from_str(toml_str).unwrap();
        assert!(matches!(config.classifier, ClassifierConfig::Simple { .. }));
        assert_eq!(config.classifier.idle_threshold_ms(), 7000);
    }

    #[test]
    fn deserialize_classifier_simple_table_defaults() {
        let toml_str = r#"
[classifier.simple]
"#;
        let config: Config = toml::from_str(toml_str).unwrap();
        assert!(matches!(config.classifier, ClassifierConfig::Simple { .. }));
        assert_eq!(config.classifier.idle_threshold_ms(), 3000);
    }

    #[test]
    fn deserialize_classifier_none_table() {
        let toml_str = r#"
[classifier.none]
"#;
        let config: Config = toml::from_str(toml_str).unwrap();
        assert!(matches!(config.classifier, ClassifierConfig::None));
    }

    /// Default classifier is Simple (general-purpose).
    #[test]
    fn default_classifier_is_simple() {
        let config = Config::default();
        assert!(matches!(config.classifier, ClassifierConfig::Simple { .. }));
        assert_eq!(config.classifier.idle_threshold_ms(), 3000);
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

[classifier.simple]
idle_threshold_ms = 5000

[[env]]
name = "FOO"
value = "bar"
"#;
        let config: Config = toml::from_str(toml_str).unwrap();
        assert_eq!(config.socket_dir, PathBuf::from("/tmp/test"));
        assert_eq!(config.scrollback_bytes, 4096);
        assert_eq!(config.session_env_var, "MY_SESSION");
        assert!(matches!(config.classifier, ClassifierConfig::Simple { .. }));
        assert_eq!(config.classifier.idle_threshold_ms(), 5000);
        assert_eq!(config.env.len(), 1);
    }

    /// Invalid classifier value produces an error.
    #[test]
    fn invalid_classifier_errors() {
        let result: Result<Config, _> = toml::from_str(r#"classifier = "bogus""#);
        assert!(result.is_err());
    }

    /// Empty TOML string produces defaults.
    #[test]
    fn empty_toml_is_defaults() {
        let config: Config = toml::from_str("").unwrap();
        assert_eq!(config.scrollback_bytes, 64 * 1024);
        assert!(matches!(config.classifier, ClassifierConfig::Simple { .. }));
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
        let config: Config = toml::from_str("scrollback_bytes = 999").unwrap();
        assert_eq!(config.scrollback_bytes, 999);
        assert_eq!(config.classifier.idle_threshold_ms(), 3000); // default
        assert!(matches!(config.classifier, ClassifierConfig::Simple { .. })); // default
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
        std::fs::write(
            &config_path,
            "[classifier.simple]\nidle_threshold_ms = 7777",
        )
        .unwrap();
        let config = load(Some(&config_path)).unwrap();
        assert_eq!(config.classifier.idle_threshold_ms(), 7777);
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

    /// Classifier name() method returns correct strings.
    #[test]
    fn classifier_name_method() {
        assert_eq!(ClassifierConfig::default().name(), "simple");
        assert_eq!(
            ClassifierConfig::Claude {
                idle_threshold_ms: 3000,
                debounce_ms: 200
            }
            .name(),
            "claude"
        );
        assert_eq!(ClassifierConfig::None.name(), "none");
    }

    /// log_file and log_level deserialize from config.
    #[test]
    fn deserialize_log_file_and_level() {
        let toml_str = r#"
log_file = "/var/log/hm.log"
log_level = "debug"
"#;
        let config: Config = toml::from_str(toml_str).unwrap();
        assert_eq!(config.log_file, Some(PathBuf::from("/var/log/hm.log")));
        assert_eq!(config.log_level, "debug");
    }

    /// log_file defaults to None, log_level defaults to "info".
    #[test]
    fn log_defaults() {
        let config: Config = toml::from_str("").unwrap();
        assert!(config.log_file.is_none());
        assert_eq!(config.log_level, "info");
    }

    /// detach_key defaults to 0x1C (Ctrl-\).
    #[test]
    fn detach_key_default() {
        let config: Config = toml::from_str("").unwrap();
        assert_eq!(config.detach_key, 0x1C);
    }

    /// detach_key can be set to 0 to disable.
    #[test]
    fn detach_key_disabled() {
        let config: Config = toml::from_str("detach_key = 0").unwrap();
        assert_eq!(config.detach_key, 0);
    }

    /// detach_key can be set to a custom value.
    #[test]
    fn detach_key_custom() {
        let config: Config = toml::from_str("detach_key = 17").unwrap();
        assert_eq!(config.detach_key, 17); // Ctrl-Q
    }

    /// Multiple classifier keys in table form is an error.
    #[test]
    fn multiple_classifier_keys_errors() {
        let toml_str = r#"
[classifier.claude]
idle_threshold_ms = 3000
[classifier.simple]
idle_threshold_ms = 3000
"#;
        let result: Result<Config, _> = toml::from_str(toml_str);
        assert!(result.is_err());
    }
}
