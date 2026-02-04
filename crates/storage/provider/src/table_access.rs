use std::cell::RefCell;

/// Database tables accessed by state provider lookups.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TableAccess {
    PlainStorageState,
    StorageChangeSets,
    StoragesHistory,
    PlainAccountState,
    AccountChangeSets,
    AccountsHistory,
    Bytecodes,
}

/// Counts of database table accesses on the current thread.
#[derive(Debug, Clone, Default)]
pub struct TableAccessCounts {
    pub plain_storage_state: u64,
    pub storage_changesets: u64,
    pub storages_history: u64,
    pub plain_account_state: u64,
    pub account_changesets: u64,
    pub accounts_history: u64,
    pub bytecodes: u64,
}

impl TableAccessCounts {
    fn increment(&mut self, table: TableAccess) {
        match table {
            TableAccess::PlainStorageState => self.plain_storage_state += 1,
            TableAccess::StorageChangeSets => self.storage_changesets += 1,
            TableAccess::StoragesHistory => self.storages_history += 1,
            TableAccess::PlainAccountState => self.plain_account_state += 1,
            TableAccess::AccountChangeSets => self.account_changesets += 1,
            TableAccess::AccountsHistory => self.accounts_history += 1,
            TableAccess::Bytecodes => self.bytecodes += 1,
        }
    }
}

#[derive(Debug, Default)]
struct TableAccessTracker {
    enabled: bool,
    counts: TableAccessCounts,
}

thread_local! {
    static TABLE_ACCESS_TRACKER: RefCell<TableAccessTracker> =
        RefCell::new(TableAccessTracker::default());
}

/// Enables table access counting on the current thread and resets counts.
pub fn enable() {
    TABLE_ACCESS_TRACKER.with(|tracker| {
        let mut tracker = tracker.borrow_mut();
        tracker.enabled = true;
        tracker.counts = TableAccessCounts::default();
    });
}

/// Disables table access counting on the current thread.
pub fn disable() {
    TABLE_ACCESS_TRACKER.with(|tracker| {
        tracker.borrow_mut().enabled = false;
    });
}

/// Resets the counts on the current thread if counting is enabled.
pub fn reset() {
    TABLE_ACCESS_TRACKER.with(|tracker| {
        let mut tracker = tracker.borrow_mut();
        if tracker.enabled {
            tracker.counts = TableAccessCounts::default();
        }
    });
}

/// Returns a snapshot of the counts if counting is enabled on the current thread.
pub fn snapshot() -> Option<TableAccessCounts> {
    TABLE_ACCESS_TRACKER.with(|tracker| {
        let tracker = tracker.borrow();
        tracker.enabled.then(|| tracker.counts.clone())
    })
}

/// Records a table access if counting is enabled on the current thread.
pub fn record(table: TableAccess) {
    TABLE_ACCESS_TRACKER.with(|tracker| {
        let mut tracker = tracker.borrow_mut();
        if !tracker.enabled {
            return;
        }
        tracker.counts.increment(table);
    });
}
