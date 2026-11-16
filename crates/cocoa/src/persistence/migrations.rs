//! Verified backups and forward-only upgrades under a whole-database schema lock.

use std::{
    fs::File,
    path::{Path, PathBuf},
    sync::Arc,
};

use sqlx::{
    Connection, FromRow, SqliteConnection,
    migrate::{MigrationType, Migrator},
};

use super::{GuardedConnection, PersistenceError, locks};

#[path = "backups.rs"]
mod backups;

pub(super) async fn open(path: &Path) -> Result<(GuardedConnection, Arc<File>), PersistenceError> {
    open_with_migrator(path, &MIGRATOR).await
}

async fn open_with_migrator(
    path: &Path,
    migrator: &Migrator,
) -> Result<(GuardedConnection, Arc<File>), PersistenceError> {
    validate_manifest(migrator)?;
    let schema = Arc::new(locks::schema(path, locks::Mode::Shared).await?);
    let mut connection = GuardedConnection::connect(path, Arc::clone(&schema), false).await?;
    if installed(connection.connection()?, migrator).await? != migrator.iter().len() {
        connection.close().await?;
        drop(schema);
        let exclusive = Arc::new(locks::schema(path, locks::Mode::Exclusive).await?);
        let mut migration = GuardedConnection::connect(path, Arc::clone(&exclusive), false).await?;
        let count = installed(migration.connection()?, migrator).await?;
        let mut backup = None;
        if count != migrator.iter().len() {
            backup = if count > 0 {
                match backups::create(path, &mut migration, &exclusive, migrator, count).await {
                    Ok(backup) => Some(backup),
                    Err(error) => {
                        inspect_failure(&mut migration, migrator).await;
                        return Err(error);
                    }
                }
            } else {
                None
            };
            if let Err(error) = sqlx::query("PRAGMA journal_mode = WAL")
                .execute(migration.connection()?)
                .await
            {
                inspect_failure(&mut migration, migrator).await;
                return Err(upgrade_error("enable WAL", backup, error.into()));
            }
            if let Err(error) = migrator
                .run_direct(None, migration.connection()?, false)
                .await
            {
                inspect_failure(&mut migration, migrator).await;
                return Err(upgrade_error("apply migrations", backup, error.into()));
            }
            let validation = async {
                if installed(migration.connection()?, migrator).await? != migrator.iter().len() {
                    return Err(PersistenceError::InvalidData(
                        "incomplete migration history",
                    ));
                }
                validate_integrity(migration.connection()?).await?;
                check_options(migration.connection()?, false).await
            }
            .await;
            if let Err(error) = validation {
                inspect_failure(&mut migration, migrator).await;
                return Err(upgrade_error("validate migrated database", backup, error));
            }
            migration.close().await.map_err(|error| {
                upgrade_error("close migration connection", backup.clone(), error)
            })?;
        } else {
            migration.close().await?;
        }
        drop(exclusive);
        let reopened = async {
            let schema = Arc::new(locks::schema(path, locks::Mode::Shared).await?);
            let mut connection =
                GuardedConnection::connect(path, Arc::clone(&schema), false).await?;
            if installed(connection.connection()?, migrator).await? != migrator.iter().len() {
                return Err(PersistenceError::InvalidData(
                    "schema changed while reacquiring shared ownership",
                ));
            }
            validate_integrity(connection.connection()?).await?;
            check_options(connection.connection()?, false).await?;
            Ok((connection, schema))
        }
        .await;
        return reopened.map_err(|error| upgrade_error("reopen migrated database", backup, error));
    }
    validate_integrity(connection.connection()?).await?;
    check_options(connection.connection()?, false).await?;
    Ok((connection, schema))
}

pub(super) async fn open_existing(
    path: &Path,
) -> Result<(GuardedConnection, Arc<File>), PersistenceError> {
    validate_manifest(&MIGRATOR)?;
    let schema = Arc::new(locks::schema(path, locks::Mode::Shared).await?);
    let mut connection = GuardedConnection::connect(path, Arc::clone(&schema), false).await?;
    let count = installed(connection.connection()?, &MIGRATOR).await?;
    if count != MIGRATOR.iter().len() {
        return Err(if count == 0 {
            PersistenceError::InvalidData("uninitialized database")
        } else {
            PersistenceError::MigrationRequired
        });
    }
    validate_integrity(connection.connection()?).await?;
    check_options(connection.connection()?, false).await?;
    Ok((connection, schema))
}

pub(super) async fn check_options(
    connection: &mut SqliteConnection,
    reader: bool,
) -> Result<(), PersistenceError> {
    let journal: String = sqlx::query_scalar("PRAGMA journal_mode")
        .fetch_one(&mut *connection)
        .await?;
    let foreign_keys: i64 = sqlx::query_scalar("PRAGMA foreign_keys")
        .fetch_one(&mut *connection)
        .await?;
    let synchronous: i64 = sqlx::query_scalar("PRAGMA synchronous")
        .fetch_one(&mut *connection)
        .await?;
    let query_only: i64 = sqlx::query_scalar("PRAGMA query_only")
        .fetch_one(&mut *connection)
        .await?;
    let busy_timeout: i64 = sqlx::query_scalar("PRAGMA busy_timeout")
        .fetch_one(&mut *connection)
        .await?;
    if journal != "wal"
        || foreign_keys != 1
        || synchronous != 2
        || query_only != i64::from(reader)
        || busy_timeout != 5000
    {
        return Err(PersistenceError::InvalidData(
            "SQLite connection durability/options",
        ));
    }
    let version: String = sqlx::query_scalar("SELECT sqlite_version()")
        .fetch_one(connection)
        .await?;
    if version != "3.51.3" {
        return Err(PersistenceError::InvalidData(
            "unexpected bundled SQLite version",
        ));
    }
    Ok(())
}

const APPLICATION_ID: i64 = 0x434f434f;
static MIGRATOR: Migrator = sqlx::migrate!("./migrations");

#[derive(FromRow)]
struct InstalledMigration {
    version: i64,
    success: i64,
    checksum: Vec<u8>,
}

#[derive(Debug, FromRow, PartialEq, Eq)]
struct SchemaItem {
    kind: String,
    name: String,
    table_name: String,
    definition: String,
}

fn validate_manifest(migrator: &Migrator) -> Result<(), PersistenceError> {
    if migrator.iter().len() == 0
        || migrator.iter().len() > 1024
        || migrator.ignore_missing
        || migrator.no_tx
        || migrator.table_name != "_sqlx_migrations"
    {
        return Err(PersistenceError::InvalidData(
            "unsupported migration manifest",
        ));
    }
    let mut previous = 0;
    for migration in migrator.iter() {
        if migration.version <= previous
            || migration.no_tx
            || migration.migration_type != MigrationType::Simple
        {
            return Err(PersistenceError::InvalidData(
                "migrations must be ordered, forward-only and transactional",
            ));
        }
        previous = migration.version;
    }
    Ok(())
}

async fn installed(
    connection: &mut SqliteConnection,
    migrator: &Migrator,
) -> Result<usize, PersistenceError> {
    let application: i64 = sqlx::query_scalar("PRAGMA application_id")
        .fetch_one(&mut *connection)
        .await?;
    let tables: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM sqlite_schema WHERE name NOT GLOB 'sqlite_*' AND name != '_sqlx_migrations'",
    ).fetch_one(&mut *connection).await?;
    let ledger: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM sqlite_schema WHERE type = 'table' AND name = '_sqlx_migrations'",
    )
    .fetch_one(&mut *connection)
    .await?;
    if application != APPLICATION_ID && !(application == 0 && tables == 0) {
        return Err(PersistenceError::InvalidData(
            "not a Cocoa session database; choose a new --data-dir",
        ));
    }
    let installed = if ledger == 1 {
        sqlx::query_as::<_, InstalledMigration>(
            "SELECT version, success, CASE WHEN length(checksum) = 48 THEN checksum ELSE x'' END AS checksum \
             FROM _sqlx_migrations ORDER BY version LIMIT 1025",
        ).fetch_all(&mut *connection).await?
    } else {
        Vec::new()
    };
    if installed.len() > migrator.iter().len() {
        return Err(PersistenceError::InvalidData("unknown migration version"));
    }
    for (installed, expected) in installed.iter().zip(migrator.iter()) {
        if installed.success != 1
            || installed.version != expected.version
            || installed.checksum != expected.checksum.as_ref()
        {
            return Err(PersistenceError::InvalidData("migration history/checksum"));
        }
    }
    if (!installed.is_empty() && application != APPLICATION_ID)
        || (installed.is_empty() && application != 0)
    {
        return Err(PersistenceError::InvalidData(
            "application identity/history",
        ));
    }
    validate_schema(connection, migrator, installed.len()).await?;
    Ok(installed.len())
}

async fn validate_schema(
    connection: &mut SqliteConnection,
    migrator: &Migrator,
    count: usize,
) -> Result<(), PersistenceError> {
    // Derive expected DDL from the embedded migrations rather than maintaining a
    // second handwritten schema. This also detects manual drift beyond checksums.
    let mut expected = SqliteConnection::connect("sqlite::memory:").await?;
    let prefix = Migrator::with_migrations(migrator.iter().take(count).cloned().collect());
    prefix.run_direct(None, &mut expected, false).await?;
    let expected_items = schema_items(&mut expected).await?;
    expected.close().await?;
    let actual = schema_items(connection).await?;
    if actual != expected_items {
        return Err(PersistenceError::InvalidData(
            "schema differs from embedded migrations",
        ));
    }
    Ok(())
}

async fn schema_items(
    connection: &mut SqliteConnection,
) -> Result<Vec<SchemaItem>, PersistenceError> {
    let bytes: i64 = sqlx::query_scalar(
        "SELECT coalesce(sum(octet_length(sql)), 0) FROM sqlite_schema WHERE name NOT GLOB 'sqlite_*'",
    ).fetch_one(&mut *connection).await?;
    if bytes > 256 * 1024 {
        return Err(PersistenceError::InvalidData("oversized schema"));
    }
    let items = sqlx::query_as(
        "SELECT type AS kind, name, tbl_name AS table_name, sql AS definition FROM sqlite_schema \
         WHERE name NOT GLOB 'sqlite_*' AND name != '_sqlx_migrations' ORDER BY type, name LIMIT 65",
    ).fetch_all(connection).await?;
    if items.len() > 64 {
        return Err(PersistenceError::InvalidData("too many schema objects"));
    }
    Ok(items)
}

async fn validate_integrity(connection: &mut SqliteConnection) -> Result<(), PersistenceError> {
    let violations = sqlx::query("PRAGMA foreign_key_check")
        .fetch_optional(&mut *connection)
        .await?;
    let integrity: String = sqlx::query_scalar("PRAGMA integrity_check(1)")
        .fetch_one(&mut *connection)
        .await?;
    let missing_root: bool = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM sessions AS session WHERE NOT EXISTS \
         (SELECT 1 FROM agents WHERE session_id = session.id AND parent_agent_id IS NULL))",
    )
    .fetch_one(connection)
    .await?;
    if violations.is_some() || integrity != "ok" || missing_root {
        return Err(PersistenceError::InvalidData(
            "database integrity or session root",
        ));
    }
    Ok(())
}

async fn inspect_failure(connection: &mut GuardedConnection, migrator: &Migrator) {
    // SQLx can fail after committing a migration. Inspect only after queued work
    // and rollback settle; never replay SQL or replace the file on this path.
    let result = async {
        connection.settle().await?;
        installed(connection.connection()?, migrator).await
    }
    .await;
    match result {
        Ok(count) => tracing::error!(
            applied_migrations = count,
            "upgrade failed; retained durable migration prefix and backup"
        ),
        Err(error) => {
            tracing::error!(%error, "upgrade failed; durable state requires inspection; retained database and backup")
        }
    }
}

fn upgrade_error(
    phase: &'static str,
    backup: Option<PathBuf>,
    source: PersistenceError,
) -> PersistenceError {
    PersistenceError::Upgrade {
        phase,
        backup,
        source: Box::new(source),
    }
}
