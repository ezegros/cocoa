//! Retained private VACUUM snapshots, published only after validation and sync.

use std::{
    fs::File,
    path::{Path, PathBuf},
    sync::Arc,
};

use sqlx::migrate::Migrator;

use super::{
    GuardedConnection, PersistenceError, installed, locks, upgrade_error, validate_integrity,
};

pub(super) async fn create(
    path: &Path,
    source: &mut GuardedConnection,
    schema: &Arc<File>,
    migrator: &Migrator,
    count: usize,
) -> Result<PathBuf, PersistenceError> {
    let root = locks::sidecar(path, ".backups");
    let guard = Arc::clone(schema);
    let pending = tokio::task::spawn_blocking(move || {
        let _guard = guard;
        prepare(&root)
    })
    .await
    .map_err(PersistenceError::from)
    .and_then(std::convert::identity)
    .map_err(|error| upgrade_error("prepare backup", None, error))?;
    let backup = pending.directory.join("database.sqlite3");
    let result = async {
        let filename = backup
            .to_str()
            .ok_or(PersistenceError::InvalidData("backup path must be UTF-8"))?;
        sqlx::query("VACUUM INTO ?")
            .bind(filename)
            .execute(source.connection()?)
            .await?;
        let filename = backup.clone();
        let guard = Arc::clone(schema);
        tokio::task::spawn_blocking(move || {
            let _guard = guard;
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                std::fs::set_permissions(&filename, std::fs::Permissions::from_mode(0o600))?;
            }
            drop(locks::private_file(
                &filename,
                locks::FileCreation::Forbidden,
            )?);
            Ok::<_, PersistenceError>(())
        })
        .await??;
        let mut verification =
            GuardedConnection::connect(&backup, Arc::clone(schema), true).await?;
        if installed(verification.connection()?, migrator).await? != count {
            return Err(PersistenceError::InvalidData("backup migration history"));
        }
        validate_integrity(verification.connection()?).await?;
        verification.close().await?;
        Ok::<_, PersistenceError>(())
    }
    .await;
    if let Err(error) = result {
        return Err(upgrade_error(
            "create and verify backup",
            Some(backup),
            error,
        ));
    }
    let guard = Arc::clone(schema);
    tokio::task::spawn_blocking(move || {
        let _guard = guard;
        publish(pending)
    })
    .await
    .map_err(|error| upgrade_error("publish backup", Some(backup), error.into()))?
}

struct PendingBackup {
    root: PathBuf,
    root_file: File,
    directory: PathBuf,
    directory_file: File,
}

fn prepare(root: &Path) -> Result<PendingBackup, PersistenceError> {
    let root_file = locks::private_directory(root)?;
    let parent = root
        .parent()
        .ok_or(PersistenceError::InvalidData("backup parent directory"))?;
    File::open(parent)?.sync_all()?;
    let mut builder = tempfile::Builder::new();
    builder.prefix("incomplete-");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        builder.permissions(std::fs::Permissions::from_mode(0o700));
    }
    let directory = builder.tempdir_in(root)?.keep();
    let directory_file = locks::private_directory(&directory)?;
    Ok(PendingBackup {
        root: root.to_owned(),
        root_file,
        directory,
        directory_file,
    })
}

fn publish(pending: PendingBackup) -> Result<PathBuf, PersistenceError> {
    let backup = pending.directory.join("database.sqlite3");
    let result = (|| {
        locks::private_file(&backup, locks::FileCreation::Forbidden)?.sync_all()?;
        pending.directory_file.sync_all()?;
        pending.root_file.sync_all()?;
        Ok::<_, PersistenceError>(())
    })();
    result.map_err(|error| upgrade_error("sync backup", Some(backup), error))?;
    let name = pending
        .directory
        .file_name()
        .and_then(|name| name.to_str())
        .and_then(|name| name.strip_prefix("incomplete-"))
        .ok_or(PersistenceError::InvalidData("backup directory name"))?;
    let verified = pending.root.join(format!("verified-{name}"));
    std::fs::rename(&pending.directory, &verified).map_err(|error| {
        upgrade_error(
            "publish backup",
            Some(pending.directory.join("database.sqlite3")),
            error.into(),
        )
    })?;
    let backup = verified.join("database.sqlite3");
    pending.root_file.sync_all().map_err(|error| {
        upgrade_error("sync published backup", Some(backup.clone()), error.into())
    })?;
    Ok(backup)
}
