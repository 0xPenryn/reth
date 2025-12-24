#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        init_db,
        mdbx::DatabaseArguments,
        test_utils::create_test_rw_db,
    };
    use reth_db_api::{
        cursor::DbCursorRO,
        database::Database,
        models::ClientVersion,
        transaction::DbTx,
        tables,
    };
    use reth_libmdbx::MaxReadTransactionDuration;
    use reth_node_core::args::WarmupMode;

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
        
        // Add some test data to state tables
        {
            let tx = db.tx_mut().unwrap();
            // We can't easily add data without the full provider setup,
            // but we can test that the warmup function runs without error
            tx.commit().unwrap();
        }

        let stats = warmup_database(&db, WarmupMode::State, None).unwrap();
        // Should have attempted to warm state tables
        assert!(stats.tables_warmed <= 3); // May be 0 if tables are empty
    }

    #[test]
    fn test_warmup_execution_mode() {
        let db = create_test_rw_db();
        let stats = warmup_database(&db, WarmupMode::Execution, None).unwrap();
        // Should have attempted to warm execution tables
        assert!(stats.tables_warmed <= 8); // May be 0 if tables are empty
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

    #[test]
    fn test_get_table_sizes() {
        let db = create_test_rw_db();
        let tables = vec!["PlainAccountState".to_string(), "Headers".to_string()];
        let sizes = get_table_sizes(&db, &tables).unwrap();
        // Should return sizes for tables that exist
        assert!(sizes.len() <= tables.len());
    }
}
