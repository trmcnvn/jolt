//! Resolve the configured data root and migrate the legacy default when safe.

use std::path::{Path, PathBuf};

use anyhow::{Context as _, bail};
use jolt_engine::InstanceLock;

const MIGRATION_MARKER: &str = ".jolt-data-root-v2";

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum MigrationOutcome {
    Ready,
    Deferred,
}

pub fn resolve() -> anyhow::Result<PathBuf> {
    if let Some(path) = std::env::var_os("JOLT_DATA_DIR")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
    {
        return Ok(path);
    }

    let target = jolt_platform::data_dir::default_data_dir()
        .context("resolving the platform data directory")?;
    let legacy = jolt_platform::data_dir::legacy_data_dir()
        .context("resolving the legacy data directory")?;
    match migrate_legacy(&legacy, &target)? {
        MigrationOutcome::Ready => Ok(target),
        MigrationOutcome::Deferred => Ok(legacy),
    }
}

fn migrate_legacy(legacy: &Path, target: &Path) -> anyhow::Result<MigrationOutcome> {
    if legacy == target {
        return Ok(MigrationOutcome::Ready);
    }

    let legacy_metadata = match std::fs::symlink_metadata(legacy) {
        Ok(metadata) => Some(metadata),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(error) => {
            return Err(error).with_context(|| format!("inspecting {}", legacy.display()));
        }
    };
    let target_metadata = match std::fs::symlink_metadata(target) {
        Ok(metadata) => Some(metadata),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(error) => {
            return Err(error).with_context(|| format!("inspecting {}", target.display()));
        }
    };

    let Some(legacy_metadata) = legacy_metadata else {
        if target.join(MIGRATION_MARKER).is_file() {
            ensure_legacy_alias(legacy, target)?;
        }
        return Ok(MigrationOutcome::Ready);
    };

    if legacy_metadata.file_type().is_symlink() {
        if symlink_points_to(legacy, target)? {
            return Ok(MigrationOutcome::Ready);
        }
        bail!(
            "legacy data path {} is a symlink to a different location; set JOLT_DATA_DIR explicitly or correct the symlink",
            legacy.display()
        );
    }
    if !legacy_metadata.is_dir() {
        bail!("legacy data path {} is not a directory", legacy.display());
    }

    if target_metadata.is_some() {
        bail!(
            "both the legacy Jolt data directory ({}) and the platform data directory ({}) exist; move or remove one before starting Jolt",
            legacy.display(),
            target.display()
        );
    }

    // A running old engine still owns this directory. Keep using it for this
    // launch; a later startup after that engine exits performs the migration.
    if InstanceLock::holder(legacy).is_some() {
        return Ok(MigrationOutcome::Deferred);
    }
    let migration_lock = match InstanceLock::acquire(legacy) {
        Ok(lock) => lock,
        Err(_error) if InstanceLock::holder(legacy).is_some() => {
            return Ok(MigrationOutcome::Deferred);
        }
        Err(error) => return Err(error.into()),
    };

    let marker = legacy.join(MIGRATION_MARKER);
    std::fs::write(&marker, target.as_os_str().as_encoded_bytes())
        .with_context(|| format!("writing migration marker in {}", legacy.display()))?;
    let parent = target
        .parent()
        .with_context(|| format!("{} has no parent directory", target.display()))?;
    std::fs::create_dir_all(parent).with_context(|| format!("creating {}", parent.display()))?;
    let move_kind = match move_data_root(legacy, target) {
        Ok(move_kind) => move_kind,
        Err(error) => {
            let _ = std::fs::remove_file(&marker);
            return Err(error);
        }
    };

    if let Err(error) = ensure_legacy_alias(legacy, target) {
        return match move_kind {
            MoveKind::Renamed => {
                let rollback = std::fs::rename(target, legacy);
                match rollback {
                    Ok(()) => Err(error),
                    Err(rollback_error) => Err(error.context(format!(
                        "migration also could not roll {} back to {}: {rollback_error}",
                        target.display(),
                        legacy.display()
                    ))),
                }
            }
            MoveKind::Copied => Err(error.context(format!(
                "Jolt data was copied successfully to {}; restart to retry creating the legacy compatibility symlink",
                target.display()
            ))),
        };
    }
    drop(migration_lock);
    Ok(MigrationOutcome::Ready)
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum MoveKind {
    Renamed,
    Copied,
}

fn move_data_root(source: &Path, target: &Path) -> anyhow::Result<MoveKind> {
    match std::fs::rename(source, target) {
        Ok(()) => Ok(MoveKind::Renamed),
        #[cfg(unix)]
        Err(error) if error.raw_os_error() == Some(libc::EXDEV) => {
            copy_data_root(source, target)?;
            Ok(MoveKind::Copied)
        }
        Err(error) => Err(error).with_context(|| {
            format!(
                "moving Jolt data from {} to {}",
                source.display(),
                target.display()
            )
        }),
    }
}

#[cfg(unix)]
fn copy_data_root(source: &Path, target: &Path) -> anyhow::Result<()> {
    let target_name = target
        .file_name()
        .with_context(|| format!("{} has no file name", target.display()))?;
    let mut staging_name = std::ffi::OsString::from(".");
    staging_name.push(target_name);
    staging_name.push(".migrating");
    let staging = target.with_file_name(staging_name);

    match std::fs::symlink_metadata(&staging) {
        Ok(metadata) if metadata.is_dir() => std::fs::remove_dir_all(&staging)
            .with_context(|| format!("removing stale migration copy {}", staging.display()))?,
        Ok(_) => std::fs::remove_file(&staging)
            .with_context(|| format!("removing stale migration copy {}", staging.display()))?,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => {
            return Err(error).with_context(|| format!("inspecting {}", staging.display()));
        }
    }

    if let Err(error) = copy_directory(source, &staging) {
        let _ = std::fs::remove_dir_all(&staging);
        return Err(error);
    }
    if let Err(error) = std::fs::rename(&staging, target) {
        let _ = std::fs::remove_dir_all(&staging);
        return Err(error).with_context(|| {
            format!(
                "committing copied Jolt data from {} to {}",
                staging.display(),
                target.display()
            )
        });
    }
    if let Err(error) = std::fs::remove_dir_all(source) {
        return Err(error).context(format!(
            "Jolt data was copied safely to {}, but the legacy directory {} could not be removed; both roots were left in place for manual recovery",
            target.display(),
            source.display()
        ));
    }
    Ok(())
}

#[cfg(unix)]
fn copy_directory(source: &Path, target: &Path) -> anyhow::Result<()> {
    let source_metadata = std::fs::symlink_metadata(source)
        .with_context(|| format!("inspecting {}", source.display()))?;
    std::fs::create_dir(target).with_context(|| format!("creating {}", target.display()))?;

    for entry in
        std::fs::read_dir(source).with_context(|| format!("reading {}", source.display()))?
    {
        let entry = entry.with_context(|| format!("reading {}", source.display()))?;
        let source_path = entry.path();
        let target_path = target.join(entry.file_name());
        let metadata = std::fs::symlink_metadata(&source_path)
            .with_context(|| format!("inspecting {}", source_path.display()))?;
        let file_type = metadata.file_type();
        if file_type.is_dir() {
            copy_directory(&source_path, &target_path)?;
        } else if file_type.is_file() {
            std::fs::copy(&source_path, &target_path).with_context(|| {
                format!(
                    "copying migration file {} to {}",
                    source_path.display(),
                    target_path.display()
                )
            })?;
        } else if file_type.is_symlink() {
            let destination = std::fs::read_link(&source_path)
                .with_context(|| format!("reading symlink {}", source_path.display()))?;
            std::os::unix::fs::symlink(destination, &target_path).with_context(|| {
                format!(
                    "copying migration symlink {} to {}",
                    source_path.display(),
                    target_path.display()
                )
            })?;
        } else {
            bail!(
                "cannot migrate unsupported filesystem entry {}",
                source_path.display()
            );
        }
    }

    std::fs::set_permissions(target, source_metadata.permissions())
        .with_context(|| format!("preserving permissions on {}", target.display()))
}

fn symlink_points_to(link: &Path, target: &Path) -> anyhow::Result<bool> {
    let destination =
        std::fs::read_link(link).with_context(|| format!("reading symlink {}", link.display()))?;
    let destination = if destination.is_absolute() {
        destination
    } else {
        link.parent()
            .unwrap_or_else(|| Path::new("."))
            .join(destination)
    };
    if destination == target {
        return Ok(true);
    }
    match (
        std::fs::canonicalize(&destination),
        std::fs::canonicalize(target),
    ) {
        (Ok(destination), Ok(target)) => Ok(destination == target),
        _ => Ok(false),
    }
}

#[cfg(unix)]
fn ensure_legacy_alias(legacy: &Path, target: &Path) -> anyhow::Result<()> {
    match std::fs::symlink_metadata(legacy) {
        Ok(_) if symlink_points_to(legacy, target)? => return Ok(()),
        Ok(_) => bail!(
            "cannot create compatibility symlink: {} exists",
            legacy.display()
        ),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => {
            return Err(error).with_context(|| format!("inspecting {}", legacy.display()));
        }
    }
    std::os::unix::fs::symlink(target, legacy).with_context(|| {
        format!(
            "creating legacy compatibility symlink {} -> {}",
            legacy.display(),
            target.display()
        )
    })
}

#[cfg(not(unix))]
fn ensure_legacy_alias(_legacy: &Path, _target: &Path) -> anyhow::Result<()> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    #[test]
    fn migrates_legacy_data_and_leaves_compatibility_alias() {
        let temp = tempfile::tempdir().unwrap();
        let legacy = temp.path().join("home/.jolt");
        let target = temp.path().join("data/jolt");
        std::fs::create_dir_all(&legacy).unwrap();
        std::fs::write(legacy.join("device-id"), "device-1").unwrap();

        assert_eq!(
            migrate_legacy(&legacy, &target).unwrap(),
            MigrationOutcome::Ready
        );
        assert_eq!(
            std::fs::read_to_string(target.join("device-id")).unwrap(),
            "device-1"
        );
        assert_eq!(
            std::fs::canonicalize(&legacy).unwrap(),
            std::fs::canonicalize(&target).unwrap()
        );
        assert!(target.join(MIGRATION_MARKER).is_file());
    }

    #[cfg(unix)]
    #[test]
    fn recreates_alias_after_interrupted_migration() {
        let temp = tempfile::tempdir().unwrap();
        let legacy = temp.path().join("home/.jolt");
        let target = temp.path().join("data/jolt");
        std::fs::create_dir_all(&target).unwrap();
        std::fs::write(target.join(MIGRATION_MARKER), b"legacy").unwrap();
        std::fs::create_dir_all(legacy.parent().unwrap()).unwrap();

        assert_eq!(
            migrate_legacy(&legacy, &target).unwrap(),
            MigrationOutcome::Ready
        );
        assert_eq!(
            std::fs::canonicalize(&legacy).unwrap(),
            std::fs::canonicalize(&target).unwrap()
        );
    }

    #[cfg(unix)]
    #[test]
    fn cross_filesystem_copy_preserves_tree_and_symlinks() {
        let temp = tempfile::tempdir().unwrap();
        let legacy = temp.path().join("home/.jolt");
        let target = temp.path().join("data/jolt");
        std::fs::create_dir_all(legacy.join("nested")).unwrap();
        let staging = target.parent().unwrap().join(".jolt.migrating");
        std::fs::create_dir_all(&staging).unwrap();
        std::fs::write(staging.join("stale"), "incomplete").unwrap();
        std::fs::write(legacy.join("nested/device-id"), "device-1").unwrap();
        std::os::unix::fs::symlink("nested/device-id", legacy.join("identity-link")).unwrap();

        copy_data_root(&legacy, &target).unwrap();

        assert!(!legacy.exists());
        assert!(!target.join("stale").exists());
        assert_eq!(
            std::fs::read_to_string(target.join("nested/device-id")).unwrap(),
            "device-1"
        );
        assert_eq!(
            std::fs::read_link(target.join("identity-link")).unwrap(),
            Path::new("nested/device-id")
        );
    }

    #[test]
    fn active_legacy_engine_defers_migration() {
        let temp = tempfile::tempdir().unwrap();
        let legacy = temp.path().join("home/.jolt");
        let target = temp.path().join("data/jolt");
        std::fs::create_dir_all(&legacy).unwrap();
        let _lock = InstanceLock::acquire(&legacy).unwrap();

        assert_eq!(
            migrate_legacy(&legacy, &target).unwrap(),
            MigrationOutcome::Deferred
        );
        assert!(legacy.is_dir());
        assert!(!target.exists());
    }

    #[test]
    fn refuses_to_merge_two_roots() {
        let temp = tempfile::tempdir().unwrap();
        let legacy = temp.path().join("home/.jolt");
        let target = temp.path().join("data/jolt");
        std::fs::create_dir_all(&legacy).unwrap();
        std::fs::create_dir_all(&target).unwrap();

        let error = migrate_legacy(&legacy, &target).unwrap_err();
        assert!(
            error
                .to_string()
                .contains("both the legacy Jolt data directory")
        );
    }
}
