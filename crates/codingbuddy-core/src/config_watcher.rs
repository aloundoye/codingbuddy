//! Config file watcher for hot-reload.
//!
//! Watches settings files for changes and signals when they need reloading.
//! Uses the `notify` crate for cross-platform filesystem events.

use notify::{Event, EventKind, RecommendedWatcher, RecursiveMode, Watcher};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, mpsc};

/// Watches config files and reports changes.
pub struct ConfigWatcher {
    _watcher: RecommendedWatcher,
    rx: Mutex<mpsc::Receiver<PathBuf>>,
}

// Safety: the Mutex protects the Receiver
unsafe impl Sync for ConfigWatcher {}

impl ConfigWatcher {
    /// Start watching the given config file paths.
    /// Only watches files that exist; silently skips missing ones.
    pub fn watch(paths: &[PathBuf]) -> anyhow::Result<Self> {
        let (tx, rx) = mpsc::channel::<PathBuf>();

        let mut watcher = notify::recommended_watcher(move |res: Result<Event, notify::Error>| {
            if let Ok(event) = res
                && matches!(
                    event.kind,
                    EventKind::Modify(_) | EventKind::Create(_) | EventKind::Remove(_)
                )
            {
                for path in event.paths {
                    let _ = tx.send(path);
                }
            }
        })?;

        for path in paths {
            if path.exists() {
                let _ = watcher.watch(path, RecursiveMode::NonRecursive);
            } else if let Some(parent) = path.parent()
                && parent.exists()
            {
                let _ = watcher.watch(parent, RecursiveMode::NonRecursive);
            }
        }

        Ok(Self {
            _watcher: watcher,
            rx: Mutex::new(rx),
        })
    }

    /// Check if any watched config file has changed since last poll.
    /// Returns the first changed path, if any.
    pub fn poll_change(&self) -> Option<PathBuf> {
        let rx = self.rx.lock().ok()?;
        let mut changed = None;
        while let Ok(path) = rx.try_recv() {
            if changed.is_none() {
                changed = Some(path);
            }
        }
        changed
    }
}

/// Create a ConfigWatcher for the standard settings file locations.
pub fn watch_settings(workspace: &Path) -> anyhow::Result<ConfigWatcher> {
    let mut paths = Vec::new();

    // User settings
    if let Some(home) = dirs_path() {
        paths.push(home.join("settings.json"));
    }

    // Project settings
    paths.push(workspace.join(".codingbuddy/settings.json"));
    paths.push(workspace.join(".codingbuddy/settings.local.json"));

    ConfigWatcher::watch(&paths)
}

fn dirs_path() -> Option<PathBuf> {
    #[cfg(target_os = "macos")]
    {
        std::env::var("HOME")
            .ok()
            .map(|h| PathBuf::from(h).join(".codingbuddy"))
    }
    #[cfg(target_os = "linux")]
    {
        std::env::var("HOME")
            .ok()
            .map(|h| PathBuf::from(h).join(".codingbuddy"))
    }
    #[cfg(target_os = "windows")]
    {
        std::env::var("APPDATA")
            .ok()
            .map(|h| PathBuf::from(h).join("codingbuddy"))
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
    {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn watch_nonexistent_paths_does_not_panic() {
        let paths = vec![PathBuf::from("/nonexistent/path/settings.json")];
        let watcher = ConfigWatcher::watch(&paths);
        assert!(watcher.is_ok());
    }

    #[test]
    fn poll_returns_none_when_no_changes() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.json");
        std::fs::write(&path, "{}").unwrap();
        let watcher = ConfigWatcher::watch(&[path]).unwrap();
        // No changes yet
        std::thread::sleep(std::time::Duration::from_millis(50));
        assert!(watcher.poll_change().is_none());
    }
}
