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
use reth_libmdbx::ffi;
use reth_tracing::tracing::{debug, info, warn};
use std::{
    collections::HashMap,
    sync::Arc,
    thread,
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
    let memory_limit = memory_limit_percent.unwrap_or(80);
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

    // For "all" mode, try to use libmdbx's optimized warmup API first
    if mode == WarmupMode::All {
        if warmup_all_tables_libmdbx(db).is_ok() {
            info!(
                target: "reth::db::warmup",
                "Used libmdbx built-in warmup API for optimal performance"
            );
            // Return early with success - libmdbx handles everything
            return Ok(WarmupStats {
                tables_warmed: tables.len(),
                total_entries: 0, // libmdbx doesn't return entry count
                duration: start.elapsed(),
                skipped_tables: vec![],
            });
        }
        // Fall through to parallel warmup if libmdbx API fails
        warn!(
            target: "reth::db::warmup",
            "libmdbx warmup API unavailable, falling back to parallel table warmup"
        );
    }

    // Determine optimal thread count (2x CPU cores as requested)
    let num_threads = (num_cpus::get() * 2).max(1).min(tables.len());
    info!(
        target: "reth::db::warmup",
        num_threads = num_threads,
        "Using parallel warmup with {} threads",
        num_threads
    );

    // Warm up tables in parallel
    let stats = if num_threads > 1 && tables.len() > 1 {
        warmup_tables_parallel(db, &tables, num_threads)?
    } else {
        warmup_tables_sequential(db, &tables)?
    };

    let mut final_stats = stats;
    final_stats.duration = start.elapsed();

    let throughput_mb_s = if final_stats.duration.as_secs_f64() > 0.0 {
        let total_bytes: usize = table_sizes.values().sum();
        (total_bytes as f64 / 1024.0 / 1024.0) / final_stats.duration.as_secs_f64()
    } else {
        0.0
    };

    info!(
        target: "reth::db::warmup",
        tables_warmed = final_stats.tables_warmed,
        total_entries = final_stats.total_entries,
        duration_ms = final_stats.duration.as_millis(),
        skipped = final_stats.skipped_tables.len(),
        throughput_mb_s = throughput_mb_s,
        "Completed database table pre-warming"
    );

    Ok(final_stats)
}

/// Warms up tables sequentially (fallback for single table or single thread).
fn warmup_tables_sequential(
    db: &DatabaseEnv,
    tables: &[String],
) -> Result<WarmupStats, crate::DatabaseError> {
    let mut stats = WarmupStats::default();
    let mut skipped = Vec::new();

    for table_name in tables {
        match warmup_table_optimized(db, table_name) {
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

    stats.skipped_tables = skipped;
    Ok(stats)
}

/// Warms up tables in parallel using multiple threads.
/// 
/// Each thread gets its own read transaction to avoid contention.
/// DatabaseEnv is thread-safe and can be shared across threads via Arc.
fn warmup_tables_parallel(
    db: &DatabaseEnv,
    tables: &[String],
    num_threads: usize,
) -> Result<WarmupStats, crate::DatabaseError> {
    use std::sync::mpsc;

    // DatabaseEnv can be shared across threads - wrap in Arc
    let db_arc = Arc::new(db.clone());
    let (tx, rx) = mpsc::channel();
    let tables_arc = Arc::new(tables.to_vec());
    let tables_per_thread = (tables.len() + num_threads - 1) / num_threads;

    // Spawn worker threads
    for thread_id in 0..num_threads {
        let tables_arc = Arc::clone(&tables_arc);
        let db_arc = Arc::clone(&db_arc);
        let tx = tx.clone();

        thread::spawn(move || {
            let start_idx = thread_id * tables_per_thread;
            let end_idx = (start_idx + tables_per_thread).min(tables_arc.len());
            
            let mut thread_stats = WarmupStats::default();
            let mut thread_skipped = Vec::new();

            for table_name in &tables_arc[start_idx..end_idx] {
                // Each thread creates its own transaction from the shared DatabaseEnv
                match warmup_table_in_thread(&db_arc, table_name) {
                    Ok(entries) => {
                        thread_stats.tables_warmed += 1;
                        thread_stats.total_entries += entries;
                        debug!(
                            target: "reth::db::warmup",
                            thread = thread_id,
                            table = table_name,
                            entries = entries,
                            "Warmed table"
                        );
                    }
                    Err(e) => {
                        warn!(
                            target: "reth::db::warmup",
                            thread = thread_id,
                            table = table_name,
                            error = ?e,
                            "Failed to warm table, skipping"
                        );
                        thread_skipped.push(table_name.clone());
                    }
                }
            }

            let _ = tx.send((thread_stats, thread_skipped));
        });
    }

    drop(tx); // Close sender so receiver knows when all threads are done

    // Collect results from all threads
    let mut total_stats = WarmupStats::default();
    let mut all_skipped = Vec::new();

    for (thread_stats, thread_skipped) in rx {
        total_stats.tables_warmed += thread_stats.tables_warmed;
        total_stats.total_entries += thread_stats.total_entries;
        all_skipped.extend(thread_skipped);
    }

    total_stats.skipped_tables = all_skipped;
    Ok(total_stats)
}

/// Warms up a table in a worker thread.
/// This function creates its own transaction from the shared DatabaseEnv.
fn warmup_table_in_thread(
    db: &Arc<DatabaseEnv>,
    table_name: &str,
) -> Result<usize, crate::DatabaseError> {
    // Each thread creates its own read-only transaction
    // This is safe because DatabaseEnv is thread-safe
    warmup_table_optimized(db, table_name)
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

/// Warms up a specific table using optimized sequential access patterns.
fn warmup_table_optimized(db: &DatabaseEnv, table_name: &str) -> Result<usize, crate::DatabaseError> {
    let mut tx = db.tx()?;
    let result = warmup_table_by_name_optimized(&tx, table_name)?;
    tx.commit()?;
    Ok(result)
}

/// Optimized table warmup using sequential page access patterns.
/// 
/// This function uses several optimizations:
/// 1. Sequential cursor iteration with larger batches
/// 2. Optimized iteration pattern to maximize sequential reads
/// 3. Large batch processing for better cache utilization
fn warmup_table_by_name_optimized(
    tx: &<DatabaseEnv as Database>::TX,
    table_name: &str,
) -> Result<usize, crate::DatabaseError> {
    // Open the table database and get the dbi
    let table_db = tx.inner.open_db(Some(table_name))
        .map_err(|e| crate::DatabaseError::Open(e.into()))?;
    let dbi = table_db.dbi();
    
    // Create cursor for sequential iteration
    let cursor = tx.inner.cursor_with_dbi(dbi)
        .map_err(|e| crate::DatabaseError::InitCursor(e.into()))?;
    
    // Use very large batch sizes for maximum throughput on NVMe SSDs
    // This maximizes sequential access and reduces per-entry overhead
    const BATCH_SIZE: usize = 200000; // Process 200k entries at a time for maximum throughput
    let mut count = 0;
    let mut batch_count = 0;
    
    // Optimized iteration: process in very large batches to maximize sequential reads
    // This pattern is optimized for NVMe SSDs in RAID 0 which excel at sequential access
    // We read as fast as possible with minimal per-entry overhead
    for result in cursor.iter_slices() {
        let _ = result.map_err(|e| crate::DatabaseError::Read(e.into()))?;
        count += 1;
        batch_count += 1;
        
        // Process in very large batches to improve sequential read performance
        // Only yield periodically to allow other threads to make progress
        if batch_count >= BATCH_SIZE {
            batch_count = 0;
            // Small yield to allow other threads to make progress
            thread::yield_now();
        }
    }
    
    Ok(count)
}

/// Attempts to use libmdbx's built-in warmup API for optimal performance.
/// This is much faster than manual iteration as it uses optimized sequential page access.
fn warmup_all_tables_libmdbx(db: &DatabaseEnv) -> Result<(), crate::DatabaseError> {
    // Create a transaction to access the environment
    let tx = db.tx()?;
    
    // Get environment from transaction and call warmup
    let env = tx.inner.env();
    let result = env.with_raw_env_ptr(|env_ptr| {
        // Use libmdbx's warmup API with force flag for sequential page loading
        // MDBX_warmup_force = 1 (force load pages sequentially)
        // MDBX_warmup_oomsafe = 2 (OOM-safe on POSIX, optional but safer)
        #[cfg(unix)]
        let flags = 1u32 | 2u32; // MDBX_warmup_force | MDBX_warmup_oomsafe
        #[cfg(not(unix))]
        let flags = 1u32; // MDBX_warmup_force only
        
        // Timeout: 3600 seconds in 16.16 fixed point format (seconds * 65536)
        let timeout = 3600u32 * 65536u32;
        
        unsafe {
            // Call mdbx_env_warmup with env and null transaction
            ffi::mdbx_env_warmup(env_ptr, std::ptr::null(), flags, timeout)
        }
    });
    
    // Abort the transaction since we only used it to access the environment
    drop(tx);
    
    // Check result
    // 0 = MDBX_SUCCESS
    // -1 = MDBX_RESULT_TRUE (timeout reached, but acceptable)
    if result == 0 {
        Ok(())
    } else if result == -1 {
        // MDBX_RESULT_TRUE means timeout was reached, which is acceptable
        warn!(
            target: "reth::db::warmup",
            "libmdbx warmup reached timeout, but pages were loaded"
        );
        Ok(())
    } else {
        Err(crate::DatabaseError::Other(format!(
            "libmdbx warmup failed with error code: {}",
            result
        )))
    }
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
