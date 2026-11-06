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
precision = 0

# true = show when the weekly window resets as a date and time ("Wed 2:30pm")
# instead of a countdown.
weekly_reset_date = false

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
#[serde(default)]
pub struct Config {
    pub refresh_mins: u64,
    pub opacity: u32,
    pub precision: usize,
    pub weekly_reset_date: bool,
    pub copilot: Service,
    pub claude: Claude,
    pub codex: Codex,
}

#[derive(Clone, Debug, PartialEq, Deserialize)]
#[serde(default)]
pub struct Service {
    pub enabled: bool,
}

#[derive(Clone, Debug, PartialEq, Deserialize)]
#[serde(default)]
pub struct Claude {
    pub enabled: bool,
    pub api: bool,
    pub browser_cookies: bool,
    pub renewal_date: Option<String>,
    pub estimate: bool,
}

#[derive(Clone, Debug, PartialEq, Deserialize)]
#[serde(default)]
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
            precision: 0,
            weekly_reset_date: false,
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
        let minutes = std::env::var("USAGE_WIDGET_REFRESH_MINS")
            .ok()
            .and_then(|v| v.trim().parse::<u64>().ok())
            .filter(|m| *m >= 1)
            .unwrap_or(self.refresh_mins.max(1));
        Duration::from_secs(60_u64.saturating_mul(minutes))
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
        Ok(text) => parse(&text),
    }
}

/// Parses the config text. Unknown keys, such as a misspelt setting, are
/// reported but do not throw away the rest of the file.
fn parse(text: &str) -> (Config, Option<String>) {
    let mut unknown = Vec::new();
    let parsed = toml::de::Deserializer::parse(text)
        .and_then(|de| serde_ignored::deserialize(de, |path| unknown.push(format!("`{path}`"))));
    match parsed {
        Ok(c) if unknown.is_empty() => (c, None),
        Ok(c) => (
            c,
            Some(format!(
                "config: unknown setting {} ignored",
                unknown.join(", ")
            )),
        ),
        Err(e) => (
            Config::default(),
            Some(format!("config error: {}", e.message())),
        ),
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

/// Sets `[claude] renewal_date`, keeping the rest of the file, comments
/// included, as it was.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
pub fn set_renewal_date(date: &str) -> Result<(), String> {
    let path = ensure_exists()?;
    let text = std::fs::read_to_string(&path)
        .map_err(|e| format!("cannot read {}: {e}", path.display()))?;
    let updated = with_renewal_date(&text, date)?;
    crate::paths::write_atomic(&path, updated.as_bytes())
        .map_err(|e| format!("cannot write {}: {e}", path.display()))
}

/// The config text with `renewal_date` set. Tries a few small text edits and
/// keeps the first that parses to exactly the old settings plus the date, so
/// an unusual layout is refused rather than rewritten.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
fn with_renewal_date(text: &str, date: &str) -> Result<String, String> {
    let mut want: toml::Table =
        toml::from_str(text).map_err(|e| format!("config error: {}", e.message()))?;
    match want
        .entry("claude")
        .or_insert_with(|| toml::Value::Table(toml::Table::new()))
    {
        toml::Value::Table(claude) => {
            claude.insert("renewal_date".into(), toml::Value::String(date.into()));
        }
        _ => return Err("the claude config must be a TOML table".into()),
    }
    let newline = if text.contains("\r\n") { "\r\n" } else { "\n" };
    let value = format!("renewal_date = \"{date}\"");
    // Replacing a line comes before adding one.
    let (mut replace, mut insert) = (Vec::new(), Vec::new());
    let mut start = 0;
    for line in text.split_inclusive('\n') {
        let end = start + line.trim_end_matches(['\r', '\n']).len();
        // An existing setting, or the template's commented-out example.
        let key = line.trim_start().trim_start_matches('#').trim_start();
        if key
            .strip_prefix("renewal_date")
            .is_some_and(|rest| rest.trim_start().starts_with('='))
        {
            replace.push(format!("{}{value}{}", &text[..start], &text[end..]));
        }
        if line.split('#').next().unwrap_or_default().trim() == "[claude]" {
            let after = start + line.len();
            let gap = if line.ends_with('\n') { "" } else { newline };
            insert.push(format!(
                "{}{gap}{value}{newline}{}",
                &text[..after],
                &text[after..]
            ));
        }
        start += line.len();
    }
    insert.push(format!("{text}{newline}[claude]{newline}{value}{newline}"));
    replace
        .into_iter()
        .chain(insert)
        .find(|c| toml::from_str::<toml::Table>(c).is_ok_and(|t| t == want))
        .ok_or_else(|| {
            "cannot safely add renewal_date to the config; add it under [claude] by hand".into()
        })
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
        // Run through the shell like git does, so the editor can be a path with
        // spaces or carry its own arguments.
        Some(editor) => Command::new("sh")
            .arg("-c")
            .arg(format!("{editor} \"$@\""))
            .arg("editor")
            .arg(&path)
            .status()
            .map(drop),
        // Waited for on another thread so it does not linger as a zombie.
        None => system_editor(&path).spawn().map(|mut child| {
            std::thread::spawn(move || child.wait());
        }),
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
    }

    #[test]
    fn unknown_keys_are_reported_and_the_rest_kept() {
        let (c, error) = parse("refresh_minutes = 1\nopacity = 50\n[claude]\nenabld = false");
        assert_eq!(c.opacity, 50);
        assert!(c.enabled(Provider::Claude));
        let error = error.unwrap();
        assert!(error.contains("`refresh_minutes`"), "{error}");
        assert!(error.contains("`claude.enabld`"), "{error}");
    }

    #[test]
    fn renewal_date_edits_keep_the_rest() {
        // The template's commented example becomes the setting.
        let set = with_renewal_date(TEMPLATE, "2026-12-01").unwrap();
        assert!(set.contains("\nrenewal_date = \"2026-12-01\"\n"), "{set}");
        assert_eq!(set.lines().count(), TEMPLATE.lines().count());
        let (c, error) = parse(&set);
        assert_eq!(error, None);
        assert_eq!(c.claude.renewal_date.as_deref(), Some("2026-12-01"));

        // An existing value is replaced, CRLF and the other settings kept.
        let crlf = "opacity = 50\r\n[claude]\r\nrenewal_date = \"2026-01-01\"\r\n";
        assert_eq!(
            with_renewal_date(crlf, "2026-12-01").unwrap(),
            "opacity = 50\r\n[claude]\r\nrenewal_date = \"2026-12-01\"\r\n"
        );

        // No [claude] table yet.
        let added = with_renewal_date("opacity = 50\n", "2026-12-01").unwrap();
        let (c, _) = parse(&added);
        assert_eq!(
            (c.opacity, c.claude.renewal_date.as_deref()),
            (50, Some("2026-12-01"))
        );

        // A key named the same in another table is left alone.
        let other = "[codex]\nrenewal_date = 1\n";
        let set = with_renewal_date(other, "2026-12-01").unwrap();
        assert!(set.starts_with(other), "{set}");

        assert!(with_renewal_date("claude = 1\n", "2026-12-01").is_err());
    }

    #[test]
    fn invalid_toml_falls_back_to_defaults() {
        let (c, error) = parse("opacity = \"high\"");
        assert_eq!(c, Config::default());
        assert!(error.unwrap().starts_with("config error"));
    }

    #[test]
    fn manual_claude_renewal() {
        let c: Config = toml::from_str("[claude]\nrenewal_date = \"2026-10-12\"").unwrap();
        assert_eq!(c.claude.renewal_date.as_deref(), Some("2026-10-12"));
        assert!(!c.claude.browser_cookies);
    }
}
