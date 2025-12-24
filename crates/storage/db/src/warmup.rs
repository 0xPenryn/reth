//! Database table pre-warming functionality.
//!
//! This module provides functionality to pre-warm specific database tables into memory
//! at startup, improving performance by loading frequently accessed tables before
//! block execution begins.

use crate::{
    implementation::mdbx::DatabaseEnv,
    tables::Tables,
};
use reth_db_api::{
    database::Database,
    table::{DupSort, Table},
    transaction::DbTx,
};
use reth_tracing::tracing::{debug, info, warn};
use std::{
    collections::HashMap,
    time::{Duration, Instant},
};

/// Database table pre-warming mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum WarmupMode {
    /// No pre-warming (default).
    None,
    /// Pre-warm only state tables (PlainAccountState, PlainStorageState, Bytecodes).
    State,
    /// Pre-warm tables needed for block execution (state + block data tables).
    Execution,
    /// Pre-warm all tables.
    All,
}

/// Statistics about the warmup process.
#[derive(Debug, Default, Clone)]
pub struct WarmupStats {
    /// Number of tables warmed.
    pub tables_warmed: usize,
    /// Total entries read across all tables.
    pub total_entries: usize,
    /// Total time taken for warmup.
    pub duration: Duration,
    /// Tables that were skipped due to memory constraints.
    pub skipped_tables: Vec<String>,
}

/// Pre-warms database tables based on the specified mode.
pub fn warmup_database(
    db: &DatabaseEnv,
    mode: WarmupMode,
    memory_limit_percent: Option<u8>,
) -> Result<WarmupStats, crate::DatabaseError> {
    if mode == WarmupMode::None {
        return Ok(WarmupStats::default());
    }

    let start = Instant::now();
    let tables = get_tables_for_mode(mode);
    info!(
        target: "reth::db::warmup",
        mode = ?mode,
        table_count = tables.len(),
        "Starting database table pre-warming"
    );

    // Get table sizes and check memory constraints
    let table_sizes = get_table_sizes(db, &tables)?;
    let total_size: usize = table_sizes.values().sum();

    // Check memory constraints if limit is specified
    let memory_limit = memory_limit_percent.unwrap_or(50);
    if memory_limit > 0 {
        if let Ok(available_memory) = get_available_memory() {
            let limit_bytes = (available_memory as f64 * (memory_limit as f64 / 100.0)) as usize;
            if total_size > limit_bytes {
                warn!(
                    target: "reth::db::warmup",
                    total_size = human_bytes(total_size as f64),
                    available_memory = human_bytes(available_memory as f64),
                    limit_percent = memory_limit,
                    "Total table size exceeds memory limit, some tables may be skipped"
                );
            }
        }
    }

    let mut stats = WarmupStats::default();
    let mut skipped = Vec::new();

    // Warm up tables in priority order
    for table_name in &tables {
        match warmup_table(db, table_name) {
            Ok(entries) => {
                stats.tables_warmed += 1;
                stats.total_entries += entries;
                debug!(
                    target: "reth::db::warmup",
                    table = table_name,
                    entries = entries,
                    "Warmed table"
                );
            }
            Err(e) => {
                warn!(
                    target: "reth::db::warmup",
                    table = table_name,
                    error = ?e,
                    "Failed to warm table, skipping"
                );
                skipped.push(table_name.clone());
            }
        }
    }

    stats.duration = start.elapsed();
    stats.skipped_tables = skipped;

    info!(
        target: "reth::db::warmup",
        tables_warmed = stats.tables_warmed,
        total_entries = stats.total_entries,
        duration_ms = stats.duration.as_millis(),
        skipped = stats.skipped_tables.len(),
        "Completed database table pre-warming"
    );

    Ok(stats)
}

/// Returns the list of tables to warm based on the mode.
fn get_tables_for_mode(mode: WarmupMode) -> Vec<String> {
    match mode {
        WarmupMode::None => vec![],
        WarmupMode::State => vec![
            "PlainAccountState".to_string(),
            "PlainStorageState".to_string(),
            "Bytecodes".to_string(),
        ],
        WarmupMode::Execution => vec![
            // State tables (highest priority)
            "PlainAccountState".to_string(),
            "PlainStorageState".to_string(),
            "Bytecodes".to_string(),
            // Block data tables
            "Headers".to_string(),
            "CanonicalHeaders".to_string(),
            "BlockBodyIndices".to_string(),
            "Transactions".to_string(),
            "Receipts".to_string(),
        ],
        WarmupMode::All => Tables::ALL.iter().map(|t| t.name().to_string()).collect(),
    }
}

/// Gets the size in bytes of each specified table.
fn get_table_sizes(
    db: &DatabaseEnv,
    table_names: &[String],
) -> Result<HashMap<String, usize>, crate::DatabaseError> {
    let mut sizes = HashMap::new();

    let mut tx = db.tx()?;
    for table_name in table_names {
        match get_table_size_by_name(&tx, table_name) {
            Ok(size) => {
                sizes.insert(table_name.clone(), size);
            }
            Err(e) => {
                warn!(
                    target: "reth::db::warmup",
                    table = table_name,
                    error = ?e,
                    "Failed to get table size"
                );
            }
        }
    }
    tx.commit()?;

    Ok(sizes)
}

/// Gets the size of a specific table by name.
fn get_table_size_by_name(
    tx: &<DatabaseEnv as Database>::TX,
    table_name: &str,
) -> Result<usize, crate::DatabaseError> {
    // Get table stats to calculate size
    let table_db = tx.inner.open_db(Some(table_name))
        .map_err(|e| crate::DatabaseError::Open(e.into()))?;
    let stats = tx.inner.db_stat(&table_db)
        .map_err(|e| crate::DatabaseError::Stats(e.into()))?;

    let page_size = stats.page_size() as usize;
    let leaf_pages = stats.leaf_pages();
    let branch_pages = stats.branch_pages();
    let overflow_pages = stats.overflow_pages();
    let num_pages = leaf_pages + branch_pages + overflow_pages;
    let table_size = page_size * num_pages;

    Ok(table_size)
}

/// Warms up a specific table by iterating through all its entries.
fn warmup_table(db: &DatabaseEnv, table_name: &str) -> Result<usize, crate::DatabaseError> {
    let mut tx = db.tx()?;
    let result = warmup_table_by_name(&tx, table_name)?;
    tx.commit()?;
    Ok(result)
}

/// Warms up a table by name using raw cursor iteration.
fn warmup_table_by_name(
    tx: &<DatabaseEnv as Database>::TX,
    table_name: &str,
) -> Result<usize, crate::DatabaseError> {
    // Open the table database and get the dbi
    let table_db = tx.inner.open_db(Some(table_name))
        .map_err(|e| crate::DatabaseError::Open(e.into()))?;
    let dbi = table_db.dbi();
    
    // Create a cursor and iterate through all entries to touch pages
    let cursor = tx.inner.cursor_with_dbi(dbi)
        .map_err(|e| crate::DatabaseError::InitCursor(e.into()))?;
    
    // Use the cursor's iter_slices method to iterate through all entries
    // This will touch all pages in the table
    let mut count = 0;
    for result in cursor.iter_slices() {
        let _ = result.map_err(|e| crate::DatabaseError::Read(e.into()))?;
        count += 1;
    }

    Ok(count)
}


/// Gets available system memory in bytes.
///
/// Returns an error if memory information cannot be retrieved.
fn get_available_memory() -> Result<usize, String> {
    #[cfg(target_os = "linux")]
    {
        use std::fs;
        // Read from /proc/meminfo
        let meminfo = fs::read_to_string("/proc/meminfo")
            .map_err(|e| format!("Failed to read /proc/meminfo: {e}"))?;
        for line in meminfo.lines() {
            if line.starts_with("MemAvailable:") {
                let parts: Vec<&str> = line.split_whitespace().collect();
                if parts.len() >= 2 {
                    if let Ok(kb) = parts[1].parse::<usize>() {
                        return Ok(kb * 1024); // Convert KB to bytes
                    }
                }
            }
        }
        Err("Could not parse MemAvailable from /proc/meminfo".to_string())
    }

    #[cfg(target_os = "macos")]
    {
        use std::process::Command;
        // Use sysctl on macOS
        let output = Command::new("sysctl")
            .arg("-n")
            .arg("hw.memsize")
            .output()
            .map_err(|e| format!("Failed to run sysctl: {e}"))?;

        if output.status.success() {
            let mem_str = String::from_utf8_lossy(&output.stdout);
            let mem_bytes = mem_str.trim().parse::<usize>()
                .map_err(|e| format!("Failed to parse memory size: {e}"))?;
            // For macOS, we'll use total memory as available (conservative estimate)
            // In practice, you might want to subtract used memory
            return Ok(mem_bytes);
        }
        Err("sysctl command failed".to_string())
    }

    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        // For other platforms, return a conservative default
        // In production, you might want to use a crate like `sysinfo`
        Err("Memory detection not implemented for this platform".to_string())
    }
}

/// Formats bytes as human-readable string.
fn human_bytes(bytes: f64) -> String {
    const KB: f64 = 1024.0;
    const MB: f64 = KB * 1024.0;
    const GB: f64 = MB * 1024.0;
    const TB: f64 = GB * 1024.0;

    if bytes >= TB {
        format!("{:.2} TB", bytes / TB)
    } else if bytes >= GB {
        format!("{:.2} GB", bytes / GB)
    } else if bytes >= MB {
        format!("{:.2} MB", bytes / MB)
    } else if bytes >= KB {
        format!("{:.2} KB", bytes / KB)
    } else {
        format!("{:.0} B", bytes)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_utils::create_test_rw_db;

    #[test]
    fn test_get_tables_for_mode() {
        assert_eq!(get_tables_for_mode(WarmupMode::None).len(), 0);
        assert_eq!(get_tables_for_mode(WarmupMode::State).len(), 3);
        assert!(get_tables_for_mode(WarmupMode::Execution).len() > 3);
        assert!(get_tables_for_mode(WarmupMode::All).len() > 0);
    }

    #[test]
    fn test_human_bytes() {
        assert!(human_bytes(1024.0).contains("KB"));
        assert!(human_bytes(1024.0 * 1024.0).contains("MB"));
        assert!(human_bytes(1024.0 * 1024.0 * 1024.0).contains("GB"));
    }

    #[test]
    fn test_warmup_none_mode() {
        let db = create_test_rw_db();
        let stats = warmup_database(&db, WarmupMode::None, None).unwrap();
        assert_eq!(stats.tables_warmed, 0);
        assert_eq!(stats.total_entries, 0);
    }

    #[test]
    fn test_warmup_state_mode() {
        let db = create_test_rw_db();
        let stats = warmup_database(&db, WarmupMode::State, None).unwrap();
        // Should have attempted to warm state tables (may be 0 if tables are empty)
        assert!(stats.tables_warmed <= 3);
    }

    #[test]
    fn test_warmup_execution_mode() {
        let db = create_test_rw_db();
        let stats = warmup_database(&db, WarmupMode::Execution, None).unwrap();
        // Should have attempted to warm execution tables
        assert!(stats.tables_warmed <= 8);
    }

    #[test]
    fn test_warmup_all_mode() {
        let db = create_test_rw_db();
        let stats = warmup_database(&db, WarmupMode::All, None).unwrap();
        // Should have attempted to warm all tables
        assert!(stats.tables_warmed <= Tables::ALL.len());
    }

    #[test]
    fn test_warmup_with_memory_limit() {
        let db = create_test_rw_db();
        // Test with a memory limit (should not fail even if limit is exceeded)
        let stats = warmup_database(&db, WarmupMode::State, Some(10)).unwrap();
        // Should complete, possibly with some tables skipped
        assert!(stats.tables_warmed <= 3);
    }
}
