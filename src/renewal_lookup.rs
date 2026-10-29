//! Looks up Claude's renewal date when asked, and stops once it finishes or
//! is cancelled. On macOS the lookup runs in the widget's own sign-in window
//! (see `renewal_webview`); elsewhere it runs the browser helper.

#[cfg_attr(target_os = "macos", allow(dead_code))]
mod process;
use crate::providers::Cycle;
use eframe::egui;
#[cfg(not(target_os = "macos"))]
use process::Control;
use std::path::PathBuf;
#[cfg(not(target_os = "macos"))]
use std::process::Command;
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
    mpsc::{self, Receiver, TryRecvError},
};
use std::thread::{self, JoinHandle};
#[cfg(not(target_os = "macos"))]
use std::time::Duration;

enum Kind {
    Thread {
        rx: Receiver<Result<Cycle, String>>,
        cancelled: Arc<AtomicBool>,
        thread: Option<JoinHandle<()>>,
    },
    #[cfg(target_os = "macos")]
    Web(Box<crate::renewal_webview::WebLookup>),
}

pub struct Lookup {
    kind: Kind,
}

impl Lookup {
    /// Raises the sign-in window above other windows, if this lookup has one.
    pub fn bring_to_front(&self) {
        #[cfg(target_os = "macos")]
        if let Kind::Web(web) = &self.kind {
            web.bring_to_front();
        }
    }

    pub fn try_recv(&mut self) -> Result<Result<Cycle, String>, TryRecvError> {
        match &mut self.kind {
            Kind::Thread { rx, .. } => rx.try_recv(),
            #[cfg(target_os = "macos")]
            Kind::Web(web) => match web.try_recv() {
                Some(result) => Ok(result),
                None => Err(TryRecvError::Empty),
            },
        }
    }

    #[cfg(test)]
    pub fn from_receiver(rx: Receiver<Result<Cycle, String>>) -> Self {
        Self {
            kind: Kind::Thread {
                rx,
                cancelled: Arc::new(AtomicBool::new(false)),
                thread: None,
            },
        }
    }
}

impl Drop for Lookup {
    // Only macOS has a second kind.
    #[cfg_attr(not(target_os = "macos"), allow(irrefutable_let_patterns))]
    fn drop(&mut self) {
        if let Kind::Thread {
            cancelled, thread, ..
        } = &mut self.kind
        {
            cancelled.store(true, Ordering::Relaxed);
            if let Some(thread) = thread.take() {
                let _ = thread.join();
            }
        }
    }
}

pub fn start(ctx: egui::Context) -> Lookup {
    #[cfg(target_os = "macos")]
    {
        let _ = &ctx;
        match crate::renewal_webview::WebLookup::new() {
            Ok(web) => {
                return Lookup {
                    kind: Kind::Web(Box::new(web)),
                };
            }
            Err(error) => {
                eprintln!("renewal sign-in window failed: {error}");
            }
        }
    }
    let (tx, rx) = mpsc::channel();
    let cancelled = Arc::new(AtomicBool::new(false));
    #[cfg(not(target_os = "macos"))]
    let control = Control {
        cancelled: cancelled.clone(),
    };
    let thread = thread::spawn(move || {
        #[cfg(not(target_os = "macos"))]
        let result = run(&control);
        #[cfg(target_os = "macos")]
        let result = Err("renewal sign-in is unavailable".into());
        let _ = tx.send(result);
        ctx.request_repaint();
    });
    Lookup {
        kind: Kind::Thread {
            rx,
            cancelled,
            thread: Some(thread),
        },
    }
}

#[cfg_attr(target_os = "macos", allow(dead_code))]
fn node_version(name: &str) -> Vec<u32> {
    name.trim_start_matches('v')
        .split('.')
        .map(|part| part.parse().unwrap_or(0))
        .collect()
}

#[cfg_attr(target_os = "macos", allow(dead_code))]
fn node_candidates() -> Vec<PathBuf> {
    let exe = if cfg!(windows) { "node.exe" } else { "node" };
    let mut paths: Vec<_> = std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default())
        .map(|p| p.join(exe))
        .collect();
    paths.extend([
        PathBuf::from("/opt/homebrew/bin/node"),
        PathBuf::from("/usr/local/bin/node"),
    ]);
    if let Some(home) = std::env::home_dir() {
        paths.extend([
            home.join(".volta/bin").join(exe),
            home.join(".fnm/aliases/default/bin").join(exe),
        ]);
        let mut versions: Vec<_> = std::fs::read_dir(home.join(".nvm/versions/node"))
            .into_iter()
            .flatten()
            .flatten()
            .map(|e| e.path())
            .collect();
        versions.sort_by_key(|path| {
            node_version(
                path.file_name()
                    .and_then(|n| n.to_str())
                    .unwrap_or_default(),
            )
        });
        paths.extend(versions.into_iter().rev().map(|p| p.join("bin/node")));
    }
    for root in [
        std::env::var_os("FNM_DIR").map(PathBuf::from),
        crate::paths::data_dir().map(|p| p.join("fnm")),
    ]
    .into_iter()
    .flatten()
    {
        paths.push(root.join("aliases/default").join(exe));
        paths.push(root.join("aliases/default/bin").join(exe));
    }
    if let Some(programs) = std::env::var_os("ProgramFiles") {
        paths.push(PathBuf::from(programs).join("nodejs/node.exe"));
    }
    paths
}

#[cfg(not(target_os = "macos"))]
fn run(control: &Control) -> Result<Cycle, String> {
    let config_path = crate::config::ensure_exists()?;
    let folder = config_path.parent().unwrap().join("renewal-helper");
    std::fs::create_dir_all(&folder).map_err(|e| e.to_string())?;
    let lock_file = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(folder.join("lookup.lock"))
        .map_err(|e| e.to_string())?;
    lock_file
        .try_lock()
        .map_err(|_| "Another renewal lookup is already running")?;
    let lock = include_str!("../scripts/package-lock.json");
    let install = std::fs::read_to_string(folder.join("installed-lock.json"))
        .ok()
        .as_deref()
        != Some(lock)
        || !folder
            .join("node_modules/playwright/package.json")
            .is_file()
        || !folder.join("node_modules/smol-toml/package.json").is_file();
    for (name, text) in [
        ("package.json", include_str!("../scripts/package.json")),
        ("package-lock.json", lock),
        (
            "claude-renewal.mjs",
            include_str!("../scripts/claude-renewal.mjs"),
        ),
    ] {
        std::fs::write(folder.join(name), text).map_err(|e| e.to_string())?;
    }
    let node = node_candidates().into_iter().find(|p| p.is_file()).ok_or(
        "Cannot locate Node.js. Install Node.js or make its executable available to the widget.",
    )?;
    let result = control.output(
        Command::new(&node).args(["-p", "process.execPath"]),
        Duration::from_secs(10),
    )?;
    let node = PathBuf::from(String::from_utf8_lossy(&result.stdout).trim());
    if install {
        let bin = node.parent().ok_or("Invalid Node.js executable path")?;
        let npm = [
            bin.join("node_modules/npm/bin/npm-cli.js"),
            bin.join("../lib/node_modules/npm/bin/npm-cli.js"),
        ]
        .into_iter()
        .find(|p| p.is_file());
        let mut command = if let Some(npm) = npm {
            let mut c = Command::new(&node);
            c.arg(npm);
            c
        } else {
            Command::new(if cfg!(windows) { "npm.cmd" } else { "npm" })
        };
        let paths = std::iter::once(bin.to_path_buf())
            .chain(std::env::split_paths(
                &std::env::var_os("PATH").unwrap_or_default(),
            ))
            .collect::<Vec<_>>();
        command.env(
            "PATH",
            std::env::join_paths(paths).map_err(|e| e.to_string())?,
        );
        control.output(
            command
                .current_dir(&folder)
                .args(["ci", "--no-audit", "--no-fund"]),
            Duration::from_secs(120),
        )?;
        std::fs::write(folder.join("installed-lock.json"), lock).map_err(|e| e.to_string())?;
    }
    control.output(
        Command::new(&node)
            .env("USAGE_WIDGET_HELPER", "1")
            .arg(folder.join("claude-renewal.mjs"))
            .arg("--config")
            .arg(&config_path),
        Duration::from_secs(450),
    )?;
    let (config, error) = crate::config::load();
    if let Some(error) = error {
        return Err(error);
    }
    let date = config
        .claude
        .renewal_date
        .as_deref()
        .ok_or("Lookup did not save a renewal date")?;
    crate::providers::manual_claude_cycle(date, chrono::Local::now().date_naive())
}

#[cfg(test)]
mod discovery_tests {
    #[test]
    fn nvm_prefers_newest_numeric_version() {
        let mut versions = ["v9.11.2", "v22.9.0", "v22.10.0", "v8.0.0"];
        versions.sort_by_key(|name| super::node_version(name));
        assert_eq!(versions.last(), Some(&"v22.10.0"));
    }
}
