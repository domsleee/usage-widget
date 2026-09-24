//! User settings in a TOML file. `usage-widget config` creates and opens it.

use crate::providers::Provider;
use serde::Deserialize;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

const TEMPLATE: &str = r#"# usage-widget settings. Restart the widget after editing.

# Minutes between refreshes (minimum 1).
refresh_mins = 5

# Window opacity in percent, 20-100.
opacity = 85

# Decimal places for percentages, 0-3. The services currently report whole
# percents, so extra places show as zeros.
precision = 1

[copilot]
enabled = true

[claude]
enabled = true
# Usage comes from Claude Code's own cache (~/.claude.json). When that is over
# 3 minutes old, ask Anthropic directly, at most every 3 minutes.
# false = never call Anthropic, only read the cache.
api = true
# macOS: read your claude.ai login cookie from Chrome, Arc, Brave or Edge to
# show the renewal day. macOS asks once for keychain access.
browser_cookies = false
# Optional next billing date (local calendar date), on any OS. Overrides browser
# lookup. Update it after each renewal; it does not assume a monthly schedule.
# renewal_date = "2026-10-12"
# Claude reports whole percents. true = estimate the part of the next percent
# from this machine's Claude Code token use (shown with "~"; it warms up over a
# few ticks and can't see claude.ai or other devices).
estimate = false

[codex]
enabled = true
# Codex reports whole percents. true = estimate the part of the next percent
# from this machine's Codex token use (shown with "~"; it can't see Codex cloud
# or other devices, so the decimals are a guess).
estimate = false
"#;

#[derive(Clone, Debug, PartialEq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    pub refresh_mins: u64,
    pub opacity: u32,
    pub precision: usize,
    pub copilot: Service,
    pub claude: Claude,
    pub codex: Codex,
}

#[derive(Clone, Debug, PartialEq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Service {
    pub enabled: bool,
}

#[derive(Clone, Debug, PartialEq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Claude {
    pub enabled: bool,
    pub api: bool,
    pub browser_cookies: bool,
    pub renewal_date: Option<String>,
    pub estimate: bool,
}

#[derive(Clone, Debug, PartialEq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Codex {
    pub enabled: bool,
    pub estimate: bool,
}

impl Default for Codex {
    fn default() -> Self {
        Self {
            enabled: true,
            estimate: false,
        }
    }
}

impl Default for Config {
    fn default() -> Self {
        Self {
            refresh_mins: 5,
            opacity: 85,
            precision: 1,
            copilot: Service::default(),
            claude: Claude::default(),
            codex: Codex::default(),
        }
    }
}

impl Default for Service {
    fn default() -> Self {
        Self { enabled: true }
    }
}

impl Default for Claude {
    fn default() -> Self {
        Self {
            enabled: true,
            api: true,
            browser_cookies: false,
            renewal_date: None,
            estimate: false,
        }
    }
}

impl Config {
    pub fn enabled(&self, p: Provider) -> bool {
        match p {
            Provider::Copilot => self.copilot.enabled,
            Provider::Claude => self.claude.enabled,
            Provider::Codex => self.codex.enabled,
        }
    }

    pub fn refresh_interval(&self) -> Duration {
        Duration::from_secs(60 * self.refresh_mins.max(1))
    }
}

pub fn path() -> Option<PathBuf> {
    crate::paths::config_dir().map(|d| d.join("usage-widget").join("config.toml"))
}

/// Loads the config. A missing file means defaults; a broken one means defaults
/// plus the error, so the widget can show it.
pub fn load() -> (Config, Option<String>) {
    let Some(path) = path() else {
        return (Config::default(), None);
    };
    match std::fs::read_to_string(&path) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => (Config::default(), None),
        Err(e) => (Config::default(), Some(format!("cannot read config: {e}"))),
        Ok(text) => match toml::from_str(&text) {
            Ok(c) => (c, None),
            Err(e) => (
                Config::default(),
                Some(format!("config error: {}", e.message())),
            ),
        },
    }
}

/// Writes the default config if there is none.
pub fn ensure_exists() -> Result<PathBuf, String> {
    use std::io::Write;
    let path = path().ok_or("cannot find the config directory")?;
    if !path.exists() {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)
                .map_err(|e| format!("cannot create {}: {e}", dir.display()))?;
        }
        match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
        {
            Ok(mut file) => file
                .write_all(TEMPLATE.as_bytes())
                .map_err(|e| e.to_string())?,
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(e) => return Err(format!("cannot write {}: {e}", path.display())),
        }
    }
    Ok(path)
}

/// Opens the config using the requested terminal editor or the system editor.
pub fn open(from_terminal: bool) -> Result<PathBuf, String> {
    let path = ensure_exists()?;
    // Release builds on Windows have no console for a terminal editor to use.
    let editor = (from_terminal && !cfg!(windows))
        .then(|| {
            std::env::var("VISUAL")
                .or_else(|_| std::env::var("EDITOR"))
                .ok()
        })
        .flatten()
        .filter(|e| !e.trim().is_empty());
    let result = match editor {
        // Terminal editors take over the console, so wait for them to exit.
        Some(editor) => {
            let mut parts = editor.split_whitespace();
            let bin = parts.next().unwrap_or_default();
            Command::new(bin).args(parts).arg(&path).status().map(drop)
        }
        None => system_editor(&path).spawn().map(drop),
    };
    result.map_err(|e| format!("cannot open {}: {e}", path.display()))?;
    Ok(path)
}

fn system_editor(path: &Path) -> Command {
    let mut cmd = if cfg!(target_os = "macos") {
        let mut c = Command::new("open");
        c.arg("-t");
        c
    } else if cfg!(windows) {
        Command::new("notepad")
    } else {
        Command::new("xdg-open")
    };
    cmd.arg(path);
    cmd
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn template_is_the_defaults() {
        assert_eq!(
            toml::from_str::<Config>(TEMPLATE).unwrap(),
            Config::default()
        );
    }

    #[test]
    fn partial_and_unknown_keys() {
        let c: Config = toml::from_str("[copilot]\nenabled = false").unwrap();
        assert!(!c.enabled(Provider::Copilot));
        assert!(c.enabled(Provider::Claude) && c.claude.api);
        assert!(toml::from_str::<Config>("refresh_minutes = 5").is_err());
    }

    #[test]
    fn manual_claude_renewal() {
        let c: Config = toml::from_str("[claude]\nrenewal_date = \"2026-10-12\"").unwrap();
        assert_eq!(c.claude.renewal_date.as_deref(), Some("2026-10-12"));
        assert!(!c.claude.browser_cookies);
    }
}
