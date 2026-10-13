//! The per-user folders the widget keeps its files in.

use std::path::PathBuf;

/// Settings: `%APPDATA%` on Windows, `~/Library/Application Support` on macOS.
pub fn config_dir() -> Option<PathBuf> {
    if cfg!(windows) {
        known_folder(KnownFolder::RoamingAppData)
    } else if cfg!(target_os = "macos") {
        Some(std::env::home_dir()?.join("Library/Application Support"))
    } else {
        env_dir("XDG_CONFIG_HOME").or_else(|| Some(std::env::home_dir()?.join(".config")))
    }
}

/// App data that should roam with the user; the same folder as settings on
/// Windows and macOS.
pub fn data_dir() -> Option<PathBuf> {
    if cfg!(any(windows, target_os = "macos")) {
        config_dir()
    } else {
        env_dir("XDG_DATA_HOME").or_else(|| Some(std::env::home_dir()?.join(".local/share")))
    }
}

/// App data kept on this machine: `%LOCALAPPDATA%` on Windows, the same folder
/// as settings on macOS.
pub fn data_local_dir() -> Option<PathBuf> {
    if cfg!(windows) {
        known_folder(KnownFolder::LocalAppData)
    } else {
        data_dir()
    }
}

/// Files that can be rebuilt: `%LOCALAPPDATA%` on Windows, `~/Library/Caches`
/// on macOS.
pub fn cache_dir() -> Option<PathBuf> {
    if cfg!(windows) {
        known_folder(KnownFolder::LocalAppData)
    } else if cfg!(target_os = "macos") {
        Some(std::env::home_dir()?.join("Library/Caches"))
    } else {
        env_dir("XDG_CACHE_HOME").or_else(|| Some(std::env::home_dir()?.join(".cache")))
    }
}

#[cfg_attr(not(windows), allow(dead_code))]
enum KnownFolder {
    RoamingAppData,
    LocalAppData,
}

/// Asks Windows where the folder is, rather than trusting `%APPDATA%` and
/// `%LOCALAPPDATA%`, which a launcher can change or leave out.
#[cfg(windows)]
fn known_folder(folder: KnownFolder) -> Option<PathBuf> {
    use std::os::windows::ffi::OsStringExt;
    use windows_sys::Win32::System::Com::CoTaskMemFree;
    use windows_sys::Win32::UI::Shell::{
        FOLDERID_LocalAppData, FOLDERID_RoamingAppData, KF_FLAG_DEFAULT, SHGetKnownFolderPath,
    };
    let id = match folder {
        KnownFolder::RoamingAppData => FOLDERID_RoamingAppData,
        KnownFolder::LocalAppData => FOLDERID_LocalAppData,
    };
    let mut path = std::ptr::null_mut();
    // SAFETY: on success Windows hands back a NUL-terminated string, which is
    // copied and then freed; on failure it must be freed too (it may be null).
    unsafe {
        let status =
            SHGetKnownFolderPath(&id, KF_FLAG_DEFAULT as u32, std::ptr::null_mut(), &mut path);
        let found = (status == 0).then(|| {
            let len = (0..).take_while(|&i| *path.add(i) != 0).count();
            PathBuf::from(std::ffi::OsString::from_wide(std::slice::from_raw_parts(
                path, len,
            )))
        });
        CoTaskMemFree(path.cast());
        found
    }
}

#[cfg(not(windows))]
fn known_folder(_folder: KnownFolder) -> Option<PathBuf> {
    None
}

fn env_dir(name: &str) -> Option<PathBuf> {
    std::env::var_os(name)
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
}
