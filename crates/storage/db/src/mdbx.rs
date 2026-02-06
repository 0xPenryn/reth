//! Helper functions for initializing and opening a database.

use crate::{is_database_empty, TableSet, Tables};
use eyre::Context;
use reth_tracing::tracing;
use std::path::Path;

pub use crate::implementation::mdbx::*;
pub use reth_libmdbx::*;

const PRE_SPLIT_DB_VERSION: u64 = 2;

fn migrate_legacy_plain_state_tables(path: &Path, args: &DatabaseArguments) -> eyre::Result<()> {
    let plain_path = DatabaseEnv::plain_state_env_path(path);
    reth_fs_util::create_dir_all(&plain_path).wrap_err_with(|| {
        format!("Could not create plain-state database directory {}", plain_path.display())
    })?;

    let main_args = DatabaseEnv::args_for_instance(args, DatabaseInstance::Main);
    let plain_args = DatabaseEnv::args_for_instance(args, DatabaseInstance::PlainState);

    let main_env = DatabaseEnv::open_inner_env(path, DatabaseEnvKind::RW, &main_args)
        .with_context(|| format!("Could not open legacy database at {}", path.display()))?;
    let plain_env = DatabaseEnv::open_inner_env(&plain_path, DatabaseEnvKind::RW, &plain_args)
        .with_context(|| {
            format!("Could not open plain-state database at {}", plain_path.display())
        })?;

    for table in PLAIN_STATE_TABLES {
        let source_tx = main_env.begin_ro_txn()?;
        let destination_tx = plain_env.begin_rw_txn()?;
        let flags =
            if table.is_dupsort() { DatabaseFlags::DUP_SORT } else { DatabaseFlags::default() };

        let destination_db = destination_tx.create_db(Some(table.name()), flags)?;
        destination_tx.clear_db(destination_db.dbi())?;

        match source_tx.open_db(Some(table.name())) {
            Ok(source_db) => {
                let mut copied = 0usize;
                for row in source_tx.cursor(source_db.dbi())?.iter_slices() {
                    let (key, value) = row?;
                    destination_tx.put(
                        destination_db.dbi(),
                        key.as_ref(),
                        value.as_ref(),
                        WriteFlags::UPSERT,
                    )?;
                    copied += 1;
                }
                tracing::info!(
                    target: "storage::db::mdbx",
                    table = table.name(),
                    copied,
                    "Migrated plain-state table"
                );
            }
            Err(Error::NotFound) => {
                tracing::info!(
                    target: "storage::db::mdbx",
                    table = table.name(),
                    "Legacy table missing, leaving destination empty"
                );
            }
            Err(err) => return Err(err.into()),
        }

        source_tx.commit()?;
        destination_tx.commit()?;
    }

    if args.lock_plain_state_in_memory() {
        if let Err(err) = DatabaseEnv::lock_plain_state_pages(&plain_env) {
            tracing::warn!(
                target: "storage::db::mdbx",
                %err,
                "Failed to lock migrated plain-state pages in RAM"
            );
        }
    }

    Ok(())
}

/// Migrates a legacy single-environment database to the split plain-state layout.
pub fn migrate_db(path: impl AsRef<Path>, args: DatabaseArguments) -> eyre::Result<()> {
    use crate::version::{
        create_db_version_file, get_db_version, DatabaseVersionError, DB_VERSION,
    };

    let path = path.as_ref();
    match get_db_version(path) {
        Ok(DB_VERSION) => return Ok(()),
        Ok(PRE_SPLIT_DB_VERSION) => (),
        Ok(version) => {
            return Err(crate::version::DatabaseVersionError::VersionMismatch { version }.into())
        }
        Err(DatabaseVersionError::MissingFile) => {
            // If the plain-state environment is already present we can directly mark as latest.
            if DatabaseEnv::plain_state_env_path(path).exists() {
                create_db_version_file(path)?;
                return Ok(());
            }
        }
        Err(err) => return Err(err.into()),
    }

    migrate_legacy_plain_state_tables(path, &args)?;
    create_db_version_file(path)?;
    Ok(())
}

/// Creates a new database at the specified path if it doesn't exist. Does NOT create tables. Check
/// [`init_db`].
pub fn create_db<P: AsRef<Path>>(path: P, args: DatabaseArguments) -> eyre::Result<DatabaseEnv> {
    use crate::version::{
        create_db_version_file, get_db_version, DatabaseVersionError, DB_VERSION,
    };

    let rpath = path.as_ref();
    if is_database_empty(rpath) {
        reth_fs_util::create_dir_all(rpath)
            .wrap_err_with(|| format!("Could not create database directory {}", rpath.display()))?;
        create_db_version_file(rpath)?;
    } else {
        match get_db_version(rpath) {
            Ok(DB_VERSION) => (),
            Ok(PRE_SPLIT_DB_VERSION) => {
                migrate_legacy_plain_state_tables(rpath, &args)?;
                create_db_version_file(rpath)?;
            }
            Ok(version) => {
                return Err(crate::version::DatabaseVersionError::VersionMismatch { version }.into())
            }
            Err(DatabaseVersionError::MissingFile) => {
                if DatabaseEnv::plain_state_env_path(rpath).exists() {
                    create_db_version_file(rpath)?;
                } else {
                    migrate_legacy_plain_state_tables(rpath, &args)?;
                    create_db_version_file(rpath)?;
                }
            }
            Err(err) => return Err(err.into()),
        }
    }

    Ok(DatabaseEnv::open(rpath, DatabaseEnvKind::RW, args)?)
}

/// Opens up an existing database or creates a new one at the specified path. Creates tables defined
/// in [`Tables`] if necessary. Read/Write mode.
pub fn init_db<P: AsRef<Path>>(path: P, args: DatabaseArguments) -> eyre::Result<DatabaseEnv> {
    init_db_for::<P, Tables>(path, args)
}

/// Opens up an existing database or creates a new one at the specified path. Creates tables defined
/// in the given [`TableSet`] if necessary. Read/Write mode.
pub fn init_db_for<P: AsRef<Path>, TS: TableSet>(
    path: P,
    args: DatabaseArguments,
) -> eyre::Result<DatabaseEnv> {
    let client_version = args.client_version().clone();
    let mut db = create_db(path, args)?;
    db.create_and_track_tables_for::<TS>()?;
    db.record_client_version(client_version)?;
    Ok(db)
}

/// Opens up an existing database. Read only mode. It doesn't create it or create tables if missing.
pub fn open_db_read_only(
    path: impl AsRef<Path>,
    args: DatabaseArguments,
) -> eyre::Result<DatabaseEnv> {
    use crate::version::check_db_version_file;

    let path = path.as_ref();
    check_db_version_file(path)?;
    DatabaseEnv::open(path, DatabaseEnvKind::RO, args)
        .with_context(|| format!("Could not open database at path: {}", path.display()))
}

/// Opens up an existing database. Read/Write mode with `WriteMap` enabled. It doesn't create it or
/// create tables if missing.
pub fn open_db(path: impl AsRef<Path>, args: DatabaseArguments) -> eyre::Result<DatabaseEnv> {
    fn open(path: &Path, args: DatabaseArguments) -> eyre::Result<DatabaseEnv> {
        let client_version = args.client_version().clone();
        let db = create_db(path, args)
            .with_context(|| format!("Could not open database at path: {}", path.display()))?;
        db.record_client_version(client_version)?;
        Ok(db)
    }
    open(path.as_ref(), args)
}
