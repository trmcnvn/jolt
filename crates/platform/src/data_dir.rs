//! Platform-native application data paths.

use std::io;
use std::path::PathBuf;

/// Jolt's default machine-local data directory.
///
/// Linux follows `XDG_DATA_HOME`, macOS uses Application Support, and Windows
/// uses Local AppData. Callers may still override this with `JOLT_DATA_DIR`.
pub fn default_data_dir() -> io::Result<PathBuf> {
    #[cfg(target_os = "linux")]
    {
        let base = env_path("XDG_DATA_HOME")
            .filter(|path| path.is_absolute())
            .unwrap_or(home_dir()?.join(".local/share"));
        Ok(base.join("jolt"))
    }

    #[cfg(target_os = "macos")]
    {
        Ok(home_dir()?.join("Library/Application Support/Jolt"))
    }

    #[cfg(target_os = "windows")]
    {
        let base = env_path("LOCALAPPDATA")
            .or_else(|| env_path("USERPROFILE").map(|home| home.join("AppData/Local")))
            .ok_or_else(|| missing_directory("LOCALAPPDATA and USERPROFILE are not set"))?;
        Ok(base.join("Jolt"))
    }

    #[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
    {
        Ok(home_dir()?.join(".local/share/jolt"))
    }
}

/// The pre-platform-native default used by existing installations.
pub fn legacy_data_dir() -> io::Result<PathBuf> {
    Ok(home_dir()?.join(".jolt"))
}

fn home_dir() -> io::Result<PathBuf> {
    env_path("HOME")
        .or_else(|| env_path("USERPROFILE"))
        .ok_or_else(|| missing_directory("HOME and USERPROFILE are not set"))
}

fn env_path(name: &str) -> Option<PathBuf> {
    std::env::var_os(name)
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
}

fn missing_directory(message: &str) -> io::Error {
    io::Error::new(io::ErrorKind::NotFound, message)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_uses_platform_application_name() {
        let path = default_data_dir().expect("default data directory");
        #[cfg(target_os = "linux")]
        assert_eq!(
            path.file_name().and_then(|name| name.to_str()),
            Some("jolt")
        );
        #[cfg(any(target_os = "macos", target_os = "windows"))]
        assert_eq!(
            path.file_name().and_then(|name| name.to_str()),
            Some("Jolt")
        );
    }

    #[test]
    fn legacy_root_is_dot_jolt() {
        let path = legacy_data_dir().expect("legacy data directory");
        assert_eq!(
            path.file_name().and_then(|name| name.to_str()),
            Some(".jolt")
        );
    }
}
