//! Simple subcommands: status, list, kill, clean.

use crate::util::{session_socket, with_runtime};
use crate::{config, protocol};
use std::path::PathBuf;
use std::time::Duration;

pub fn status(id: String, socket_dir: PathBuf, _cfg: &config::Config) -> anyhow::Result<()> {
    let socket_path = session_socket(&id, &socket_dir);

    with_runtime(async move {
        let mut sess = protocol::Session::connect(&socket_path).await?;
        let status = sess.recv_status().await?;

        let state_name = protocol::state_name(status.state);

        println!("session:    {id}");
        println!("pid:        {}", status.pid);
        println!("idle_ms:    {}", status.idle_ms);
        println!("alive:      {}", status.alive);
        println!("state:      {state_name} ({}ms)", status.state_ms);

        Ok(())
    })
}

pub fn list(socket_dir: PathBuf) -> anyhow::Result<()> {
    if !socket_dir.exists() {
        println!("No sessions directory found at {}", socket_dir.display());
        return Ok(());
    }

    let mut sessions: Vec<String> = std::fs::read_dir(&socket_dir)?
        .filter_map(|entry| {
            let entry = entry.ok()?;
            let path = entry.path();
            if path.extension().is_some_and(|ext| ext == "sock") {
                path.file_stem().and_then(|s| s.to_str()).map(String::from)
            } else {
                None
            }
        })
        .collect();

    sessions.sort();

    // Filter to live sessions: check PID file liveness, clean up stale entries.
    let mut live = Vec::new();
    for id in &sessions {
        let pid_path = crate::util::pid_path(&socket_dir, id);
        let socket_path = crate::util::socket_path(&socket_dir, id);

        let is_alive = crate::pidfile::PidFile::read(&pid_path).is_some_and(|pf| pf.any_alive());

        if is_alive {
            live.push(id.clone());
        } else {
            let _ = std::fs::remove_file(&socket_path);
            let _ = std::fs::remove_file(&pid_path);
        }
    }

    if live.is_empty() {
        println!("No active sessions");
    } else {
        println!("Active sessions:");
        for id in &live {
            println!("  {id}");
        }
    }

    Ok(())
}

pub fn kill(id: String, socket_dir: PathBuf) -> anyhow::Result<()> {
    let socket_path = session_socket(&id, &socket_dir);

    with_runtime(async move {
        let mut sess = protocol::Session::connect(&socket_path).await?;

        sess.send_kill().await?;
        println!("Kill signal sent to session {id}");

        Ok(())
    })
}

/// Parse a human-friendly duration string like "24h", "7d", "30m", "2h30m".
fn parse_duration(s: &str) -> anyhow::Result<Duration> {
    let mut total_secs: u64 = 0;
    let mut num_buf = String::new();

    for ch in s.chars() {
        if ch.is_ascii_digit() {
            num_buf.push(ch);
        } else {
            let n: u64 = num_buf
                .parse()
                .map_err(|_| anyhow::anyhow!("invalid duration: {s}"))?;
            num_buf.clear();
            total_secs += match ch {
                's' => n,
                'm' => n * 60,
                'h' => n * 3600,
                'd' => n * 86400,
                _ => anyhow::bail!("unknown duration unit '{ch}' in \"{s}\" (use s/m/h/d)"),
            };
        }
    }

    if !num_buf.is_empty() {
        anyhow::bail!("trailing number without unit in \"{s}\" (use s/m/h/d)");
    }
    if total_secs == 0 {
        anyhow::bail!("duration must be greater than zero: \"{s}\"");
    }

    Ok(Duration::from_secs(total_secs))
}

pub fn clean(socket_dir: PathBuf, older_than: &str, dry_run: bool) -> anyhow::Result<()> {
    let max_age = parse_duration(older_than)?;

    if !socket_dir.exists() {
        println!("No sessions directory found at {}", socket_dir.display());
        return Ok(());
    }

    let now = std::time::SystemTime::now();
    let mut removed = 0u32;
    let mut skipped_live = 0u32;
    let mut skipped_young = 0u32;

    for entry in std::fs::read_dir(&socket_dir)? {
        let entry = entry?;
        let path = entry.path();

        // Only consider .log files.
        if path.extension().is_none_or(|ext| ext != "log") {
            continue;
        }

        let id = match path.file_stem().and_then(|s| s.to_str()) {
            Some(id) => id.to_string(),
            None => continue,
        };

        // Skip if there's a live session for this ID.
        let pid_path = crate::util::pid_path(&socket_dir, &id);
        if crate::pidfile::PidFile::read(&pid_path).is_some_and(|pf| pf.any_alive()) {
            skipped_live += 1;
            continue;
        }

        // Skip if modified within the retention window.
        if let Ok(meta) = entry.metadata()
            && let Ok(modified) = meta.modified()
            && let Ok(age) = now.duration_since(modified)
            && age < max_age
        {
            skipped_young += 1;
            continue;
        }

        if dry_run {
            println!("would remove: {}", path.display());
        } else {
            match std::fs::remove_file(&path) {
                Ok(()) => {
                    println!("removed: {}", path.display());
                    removed += 1;
                }
                Err(e) => {
                    eprintln!("warning: failed to remove {}: {e}", path.display());
                }
            }
        }
    }

    if dry_run {
        println!(
            "Dry run complete. {} live, {} within retention window.",
            skipped_live, skipped_young
        );
    } else if removed == 0 {
        println!("Nothing to clean.");
    } else {
        println!(
            "Cleaned {removed} log file{}.",
            if removed == 1 { "" } else { "s" }
        );
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_duration_hours() {
        assert_eq!(parse_duration("24h").unwrap(), Duration::from_secs(86400));
    }

    #[test]
    fn parse_duration_days() {
        assert_eq!(parse_duration("7d").unwrap(), Duration::from_secs(604800));
    }

    #[test]
    fn parse_duration_minutes() {
        assert_eq!(parse_duration("30m").unwrap(), Duration::from_secs(1800));
    }

    #[test]
    fn parse_duration_seconds() {
        assert_eq!(parse_duration("90s").unwrap(), Duration::from_secs(90));
    }

    #[test]
    fn parse_duration_compound() {
        assert_eq!(
            parse_duration("2h30m").unwrap(),
            Duration::from_secs(2 * 3600 + 30 * 60)
        );
    }

    #[test]
    fn parse_duration_compound_days_hours() {
        assert_eq!(
            parse_duration("1d12h").unwrap(),
            Duration::from_secs(86400 + 12 * 3600)
        );
    }

    #[test]
    fn parse_duration_trailing_number_errors() {
        assert!(parse_duration("24").is_err());
    }

    #[test]
    fn parse_duration_unknown_unit_errors() {
        assert!(parse_duration("24w").is_err());
    }

    #[test]
    fn parse_duration_empty_errors() {
        assert!(parse_duration("").is_err());
    }

    #[test]
    fn parse_duration_zero_errors() {
        assert!(parse_duration("0h").is_err());
    }
}
