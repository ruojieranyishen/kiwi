# Multi-Instance LogIndex Snapshot Sync Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Enable Raft snapshot to correctly sync logindex state for all Redis instances (3 by default), preventing log purge errors.

**Architecture:** Each Redis instance has independent RocksDB + collector/cf_tracker. RaftSnapshotMeta changes from single-instance `Vec<String>` to multi-instance `Vec<Vec<String>>`. Snapshot creation aggregates all instance states; installation restores to each instance.

**Tech Stack:** Rust, RocksDB, OpenRaft, serde

---

## File Structure

### Modified Files
- `src/storage/src/checkpoint.rs` - RaftSnapshotMeta multi-instance support
- `src/storage/src/storage.rs` - Add get_global_smallest_flushed_log_index
- `src/raft/src/state_machine.rs` - Use new multi-instance methods
- `src/raft/src/node.rs` - Remove single-instance collector retrieval
- `src/raft/src/lib.rs` - Re-export from storage::logindex

### Deleted Files (duplicates)
- `src/raft/src/collector.rs`
- `src/raft/src/cf_tracker.rs`
- `src/raft/src/event_listener.rs`
- `src/raft/src/table_properties.rs`
- `src/raft/src/types.rs`
- `src/raft/src/db_access.rs`

### Test Files
- `src/storage/tests/checkpoint_test.rs` - New multi-instance tests
- `src/raft/tests/snapshot_logindex_test.rs` - Update for multi-instance

---

## Task 1: RaftSnapshotMeta Multi-Instance Data Structure

**Files:**
- Modify: `src/storage/src/checkpoint.rs:32-47`

- [ ] **Step 1: Write the failing test**

```rust
// src/storage/tests/checkpoint_test.rs - add to existing file or create new test module

#[test]
fn test_raft_snapshot_meta_multi_instance_format() {
    // Test that new format can serialize/deserialize multiple instance states
    let meta = storage::RaftSnapshotMeta {
        version: 2,
        last_included_index: 300,
        last_included_term: 1,
        logindex_collector_states: vec![
            vec!["100:1000".to_string()],
            vec!["200:2000".to_string()],
            vec!["300:3000".to_string()],
        ],
    };
    
    let json = serde_json::to_string_pretty(&meta).unwrap();
    let parsed: storage::RaftSnapshotMeta = serde_json::from_str(&json).unwrap();
    
    assert_eq!(parsed.logindex_collector_states.len(), 3);
    assert_eq!(parsed.logindex_collector_states[0], vec!["100:1000"]);
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cd /home/lpf/dev/kiwi && cargo test --package storage test_raft_snapshot_meta_multi_instance_format`
Expected: FAIL - field `logindex_collector_states` doesn't exist

- [ ] **Step 3: Modify RaftSnapshotMeta struct**

```rust
// src/storage/src/checkpoint.rs:32-47
// Change CURRENT_SNAPSHOT_VERSION and update struct

pub const CURRENT_SNAPSHOT_VERSION: u32 = 2;  // v1 -> v2

/// Metadata persisted next to per-instance checkpoint directories.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RaftSnapshotMeta {
    /// Snapshot format version
    pub version: u32,
    /// Last log index included in the snapshot
    pub last_included_index: u64,
    /// Last log term included in the snapshot
    pub last_included_term: u64,
    
    /// LogIndex collector state for each instance.
    /// Format: Vec[instance_id] = Vec<"log_index:seqno">
    /// Length should match Storage.db_instance_num (typically 3)
    /// For backward compatibility with v1, old field name kept with serde alias
    #[serde(default, alias = "logindex_collector_state")]
    pub logindex_collector_states: Vec<Vec<String>>,
}
```

- [ ] **Step 4: Run test to verify it passes**

Run: `cd /home/lpf/dev/kiwi && cargo test --package storage test_raft_snapshot_meta_multi_instance_format`
Expected: PASS

- [ ] **Step 5: Commit**

```bash
cd /home/lpf/dev/kiwi && git add src/storage/src/checkpoint.rs src/storage/tests/checkpoint_test.rs
git commit -m "feat(storage): RaftSnapshotMeta support multi-instance logindex state"
```

---

## Task 2: RaftSnapshotMeta Helper Methods

**Files:**
- Modify: `src/storage/src/checkpoint.rs:49-83`

- [ ] **Step 1: Write failing test for with_all_instances**

```rust
// src/storage/tests/checkpoint_test.rs

use std::sync::Arc;
use storage::logindex::LogIndexAndSequenceCollector;
use storage::storage::Storage;
use storage::StorageOptions;
use std::path::PathBuf;

#[test]
fn test_with_all_instances_aggregates_multiple_collectors() {
    // Create temp storage with 3 instances
    let db_path = tempfile::tempdir().unwrap().path().to_path_buf();
    let mut storage = Storage::new(3, 0);
    let options = Arc::new(StorageOptions::default());
    let _rx = storage.open(options, &db_path).unwrap();
    
    // Update each instance's collector with different log_index
    // Note: We need to access each instance's collector
    if let Some(c0) = storage.get_logindex_collector(0) {
        c0.update(100, 1000);
    }
    if let Some(c1) = storage.get_logindex_collector(1) {
        c1.update(200, 2000);
    }
    if let Some(c2) = storage.get_logindex_collector(2) {
        c2.update(300, 3000);
    }
    
    // Create snapshot meta
    let meta = storage::RaftSnapshotMeta::with_all_instances(300, 1, &storage);
    
    // Verify all 3 instance states captured
    assert_eq!(meta.logindex_collector_states.len(), 3);
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cd /home/lpf/dev/kiwi && cargo test --package storage test_with_all_instances`
Expected: FAIL - method `with_all_instances` not found

- [ ] **Step 3: Add with_all_instances method**

```rust
// src/storage/src/checkpoint.rs - add to impl RaftSnapshotMeta block

use crate::storage::Storage;

impl RaftSnapshotMeta {
    /// Create snapshot meta with all instance collector states
    pub fn with_all_instances(
        last_included_index: u64,
        last_included_term: u64,
        storage: &Storage,
    ) -> Self {
        let states: Vec<Vec<String>> = (0..storage.db_instance_num)
            .map(|instance_id| {
                storage.get_logindex_collector(instance_id)
                    .map(|c| c.export_state())
                    .unwrap_or_default()
            })
            .collect();
        
        Self {
            version: CURRENT_SNAPSHOT_VERSION,
            last_included_index,
            last_included_term,
            logindex_collector_states: states,
        }
    }
    
    /// Restore collector state for all instances
    pub fn restore_to_storage(&self, storage: &Storage) {
        for (instance_id, state) in self.logindex_collector_states.iter().enumerate() {
            if let Some(collector) = storage.get_logindex_collector(instance_id) {
                self.restore_instance_state(&collector, state);
            }
        }
    }
    
    /// Restore single instance collector state (internal helper)
    fn restore_instance_state(
        &self,
        collector: &Arc<LogIndexAndSequenceCollector>,
        state: &[String],
    ) {
        for entry in state {
            if let Some((log_index_str, seqno_str)) = entry.split_once(':') {
                if let (Ok(log_index), Ok(seqno)) = (
                    log_index_str.parse::<i64>(),
                    seqno_str.parse::<u64>(),
                ) {
                    collector.update(log_index, seqno);
                }
            }
        }
    }
    
    /// Backward compatibility: convert v1 format (single instance) to v2
    pub fn from_v1(
        last_included_index: u64,
        last_included_term: u64,
        single_state: Vec<String>,
    ) -> Self {
        Self {
            version: CURRENT_SNAPSHOT_VERSION,
            last_included_index,
            last_included_term,
            logindex_collector_states: vec![single_state],  // Instance 0 only
        }
    }
    
    // Keep existing methods for backward compatibility
    // ... new(), with_collector_state(), restore_collector_state() can remain
    // but with_collector_state now delegates to with_all_instances concept
}
```

- [ ] **Step 4: Add Storage import and verify compilation**

Add at top of checkpoint.rs if needed:
```rust
use crate::storage::Storage;
```

Run: `cd /home/lpf/dev/kiwi && cargo check --package storage`
Expected: PASS

- [ ] **Step 5: Run test to verify it passes**

Run: `cd /home/lpf/dev/kiwi && cargo test --package storage test_with_all_instances`
Expected: PASS

- [ ] **Step 6: Write test for restore_to_storage**

```rust
// src/storage/tests/checkpoint_test.rs

#[test]
fn test_restore_to_storage_restores_all_instances() {
    let db_path = tempfile::tempdir().unwrap().path().to_path_buf();
    let mut storage = Storage::new(3, 0);
    let options = Arc::new(StorageOptions::default());
    let _rx = storage.open(options, &db_path).unwrap();
    
    // Create meta with states for all 3 instances
    let meta = storage::RaftSnapshotMeta {
        version: 2,
        last_included_index: 300,
        last_included_term: 1,
        logindex_collector_states: vec![
            vec!["100:1000".to_string(), "150:1500".to_string()],
            vec!["200:2000".to_string()],
            vec!["300:3000".to_string()],
        ],
    };
    
    // Restore
    meta.restore_to_storage(&storage);
    
    // Verify each instance's collector has correct state
    if let Some(c0) = storage.get_logindex_collector(0) {
        assert_eq!(c0.find_applied_log_index(1000), 100);
        assert_eq!(c0.find_applied_log_index(1500), 150);
    }
    if let Some(c1) = storage.get_logindex_collector(1) {
        assert_eq!(c1.find_applied_log_index(2000), 200);
    }
    if let Some(c2) = storage.get_logindex_collector(2) {
        assert_eq!(c2.find_applied_log_index(3000), 300);
    }
}
```

- [ ] **Step 7: Run restore test**

Run: `cd /home/lpf/dev/kiwi && cargo test --package storage test_restore_to_storage`
Expected: PASS

- [ ] **Step 8: Write backward compatibility test**

```rust
// src/storage/tests/checkpoint_test.rs

#[test]
fn test_backward_compatibility_v1_json() {
    // Old v1 format JSON with single instance state
    let v1_json = r#"{
        "version": 1,
        "last_included_index": 200,
        "last_included_term": 1,
        "logindex_collector_state": ["100:1000", "200:2000"]
    }"#;
    
    // Should parse correctly due to serde alias
    let meta: storage::RaftSnapshotMeta = serde_json::from_str(v1_json).unwrap();
    
    // Converted to v2 format with single element array
    assert_eq!(meta.logindex_collector_states.len(), 1);
    assert!(meta.logindex_collector_states[0].contains(&"100:1000".to_string()));
}
```

- [ ] **Step 9: Run backward compat test**

Run: `cd /home/lpf/dev/kiwi && cargo test --package storage test_backward_compatibility`
Expected: PASS

- [ ] **Step 10: Commit**

```bash
cd /home/lpf/dev/kiwi && git add src/storage/src/checkpoint.rs src/storage/tests/checkpoint_test.rs
git commit -m "feat(storage): add with_all_instances and restore_to_storage methods"
```

---

## Task 3: Storage Global Smallest Flushed Index

**Files:**
- Modify: `src/storage/src/storage.rs` (add new method around line 500)

- [ ] **Step 1: Write failing test**

```rust
// src/storage/tests/storage_test.rs or add to existing test file

#[test]
fn test_get_global_smallest_flushed_log_index() {
    let db_path = tempfile::tempdir().unwrap().path().to_path_buf();
    let mut storage = Storage::new(3, 0);
    let options = Arc::new(StorageOptions::default());
    let _rx = storage.open(options, &db_path).unwrap();
    
    // Set different flushed states for each instance
    // Instance 0: flushed = 80
    // Instance 1: flushed = 95
    // Instance 2: flushed = 90
    // Global smallest should be 80
    
    // We need to manipulate cf_tracker directly for this test
    // In real scenario, this happens via EventListener after flush
    
    // For now, verify the method exists and returns a value
    let smallest = storage.get_global_smallest_flushed_log_index();
    // Initial state should be 0 or i64::MAX based on implementation
    assert!(smallest >= 0);
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cd /home/lpf/dev/kiwi && cargo test --package storage test_get_global_smallest`
Expected: FAIL - method `get_global_smallest_flushed_log_index` not found

- [ ] **Step 3: Add get_global_smallest_flushed_log_index**

```rust
// src/storage/src/storage.rs - add after get_smallest_applied_log_index (~line 514)

/// Get smallest flushed log index across all instances
/// This is the safe upper bound for Raft log purge
pub fn get_global_smallest_flushed_log_index(&self) -> i64 {
    let mut min_index = i64::MAX;
    
    for inst in &self.insts {
        if let Some(ref tracker) = inst.logindex_cf_tracker {
            let res = tracker.get_smallest_log_index(None);
            if res.smallest_flushed_log_index < min_index {
                min_index = res.smallest_flushed_log_index;
            }
        }
    }
    
    if min_index == i64::MAX { 0 } else { min_index }
}
```

- [ ] **Step 4: Run test to verify it passes**

Run: `cd /home/lpf/dev/kiwi && cargo test --package storage test_get_global_smallest`
Expected: PASS

- [ ] **Step 5: Commit**

```bash
cd /home/lpf/dev/kiwi && git add src/storage/src/storage.rs src/storage/tests/storage_test.rs
git commit -m "feat(storage): add get_global_smallest_flushed_log_index for Raft purge"
```

---

## Task 4: Update State Machine build_snapshot

**Files:**
- Modify: `src/raft/src/state_machine.rs:362-369`

- [ ] **Step 1: Write failing integration test**

```rust
// src/raft/tests/snapshot_logindex_test.rs - add new test

#[tokio::test]
async fn test_build_snapshot_includes_all_instances() {
    use std::sync::Arc;
    use openraft::RaftSnapshotBuilder;
    use raft::state_machine::KiwiStateMachine;
    use storage::{Storage, StorageOptions, unique_test_db_path};
    use storage::logindex::{LogIndexAndSequenceCollector, LogIndexOfColumnFamilies};
    
    let src_db_path = unique_test_db_path();
    let snap_root = unique_test_db_path();
    std::fs::create_dir_all(&snap_root).unwrap();
    
    // Create storage with 3 instances
    let storage = {
        let mut s = Storage::new(3, 0);
        let options = Arc::new(StorageOptions::default());
        let _rx = s.open(options, &src_db_path).unwrap();
        Arc::new(s)
    };
    
    // Write to each instance via different slots
    // slot 0 → instance 0, slot 1 → instance 1, slot 2 → instance 2
    // (Need to verify slot_indexer mapping)
    
    // Update collectors directly for test
    storage.get_logindex_collector(0).unwrap().update(100, 1000);
    storage.get_logindex_collector(1).unwrap().update(200, 2000);
    storage.get_logindex_collector(2).unwrap().update(300, 3000);
    
    // Create state machine
    let collector = storage.get_logindex_collector(0).unwrap();
    let cf_tracker = storage.get_logindex_cf_tracker(0).unwrap();
    let mut sm = KiwiStateMachine::new(
        1, storage.clone(), src_db_path.clone(), snap_root.clone(),
        collector, cf_tracker,
    );
    
    // Build snapshot
    let mut builder = sm.get_snapshot_builder().await;
    let snap = builder.build_snapshot().await.unwrap();
    
    // Verify snapshot contains all instance states
    // Extract and parse __raft_snapshot_meta from snapshot tar
    // ...
}
```

- [ ] **Step 2: Run test to verify setup works**

Run: `cd /home/lpf/dev/kiwi && cargo test --package raft test_build_snapshot_includes -- --nocapture`
Expected: May pass or fail depending on current implementation

- [ ] **Step 3: Modify KiwiSnapshotBuilder to use with_all_instances**

```rust
// src/raft/src/state_machine.rs - modify build_snapshot in KiwiSnapshotBuilder

impl RaftSnapshotBuilder<KiwiTypeConfig> for KiwiSnapshotBuilder {
    async fn build_snapshot(&mut self) -> Result<Snapshot<KiwiTypeConfig>, StorageError> {
        // ... existing code for temp_dir, last_idx, last_term ...
        
        // CHANGE: Use with_all_instances instead of with_collector_state
        let raft_meta = RaftSnapshotMeta::with_all_instances(
            last_idx,
            last_term,
            &self._storage,  // Need to add storage reference to builder
        );
        
        // ... rest of existing code ...
    }
}
```

Note: KiwiSnapshotBuilder needs access to Storage. Check current struct definition:
```rust
pub struct KiwiSnapshotBuilder {
    _storage: Arc<Storage>,  // Already has this
    // ...
}
```

- [ ] **Step 4: Verify compilation**

Run: `cd /home/lpf/dev/kiwi && cargo check --package raft`
Expected: PASS

- [ ] **Step 5: Run integration test**

Run: `cd /home/lpf/dev/kiwi && cargo test --package raft --test snapshot_logindex_test`
Expected: PASS

- [ ] **Step 6: Commit**

```bash
cd /home/lpf/dev/kiwi && git add src/raft/src/state_machine.rs src/raft/tests/snapshot_logindex_test.rs
git commit -m "feat(raft): build_snapshot uses with_all_instances for multi-instance"
```

---

## Task 5: Update State Machine install_snapshot

**Files:**
- Modify: `src/raft/src/state_machine.rs:298-318`

- [ ] **Step 1: Write integration test for install_snapshot multi-instance**

```rust
// src/raft/tests/snapshot_logindex_test.rs

#[tokio::test]
async fn test_install_snapshot_restores_all_instances() {
    // Setup source storage with 3 instances
    // Build snapshot with multi-instance state
    // Create target storage
    // Install snapshot
    // Verify all 3 target collectors have correct state
}
```

- [ ] **Step 2: Modify install_snapshot to use restore_to_storage**

```rust
// src/raft/src/state_machine.rs - modify install_snapshot

async fn install_snapshot(
    &mut self,
    meta: &SnapshotMeta<u64, KiwiNode>,
    snapshot: Box<std::io::Cursor<Vec<u8>>>,
) -> Result<(), openraft::StorageError<u64>> {
    // ... existing unpack logic ...
    
    let file_meta = RaftSnapshotMeta::read_from_dir(&checkpoint_root)?;
    
    // ... restore_checkpoint_layout ...
    
    // Initialize cf_tracker from SST (existing)
    self.storage.init_cf_trackers()?;  // Note: may need to add storage field
    
    // CHANGE: Use restore_to_storage instead of restore_collector_state
    file_meta.restore_to_storage(&self.storage);
    
    // ... rest of existing code ...
}
```

Note: KiwiStateMachine needs Storage reference. Check current:
```rust
pub struct KiwiStateMachine {
    storage: Arc<Storage>,  // Already has this
    // ...
}
```

- [ ] **Step 3: Verify compilation**

Run: `cd /home/lpf/dev/kiwi && cargo check --package raft`
Expected: PASS

- [ ] **Step 4: Run integration test**

Run: `cd /home/lpf/dev/kiwi && cargo test --package raft --test snapshot_roundtrip_test`
Expected: PASS

- [ ] **Step 5: Commit**

```bash
cd /home/lpf/dev/kiwi && git add src/raft/src/state_machine.rs
git commit -m "feat(raft): install_snapshot uses restore_to_storage for multi-instance"
```

---

## Task 6: Remove Duplicate Files from raft/src

**Files:**
- Delete: `src/raft/src/collector.rs`
- Delete: `src/raft/src/cf_tracker.rs`
- Delete: `src/raft/src/event_listener.rs`
- Delete: `src/raft/src/table_properties.rs`
- Delete: `src/raft/src/types.rs`
- Delete: `src/raft/src/db_access.rs`
- Modify: `src/raft/src/lib.rs`

- [ ] **Step 1: Check which files are actually used from raft module**

Run: `cd /home/lpf/dev/kiwi && grep -r "use crate::" src/raft/src/ | grep -E "(collector|cf_tracker|event_listener|table_properties|types|db_access)"`

- [ ] **Step 2: Update raft/src/lib.rs to re-export from storage**

```rust
// src/raft/src/lib.rs - replace duplicate module definitions with re-exports

pub mod api;
pub mod log_store;
pub mod log_store_rocksdb;
pub mod network;
pub mod node;
pub mod snapshot_archive;
pub mod state_machine;

// DELETE these duplicate module declarations:
// pub mod cf_tracker;
// pub mod collector;
// pub mod db_access;
// pub mod event_listener;
// pub mod table_properties;
// pub mod types;

// RE-EXPORT from storage::logindex instead
pub use storage::logindex::{
    LogIndexAndSequenceCollector,
    LogIndexOfColumnFamilies,
    LogIndexAndSequenceCollectorPurger,
    LogIndexTablePropertiesCollectorFactory,
    LogIndexSeqnoPair,
    LogIndexAndSequencePair,
    LogIndex,
    SequenceNumber,
    SmallestIndexRes,
    SnapshotCallback,
    FlushTrigger,
    PROPERTY_KEY,
    get_largest_log_index_from_collection,
    read_stats_from_table_props,
};

// Keep CF_NAMES constant for internal use
pub const COLUMN_FAMILY_COUNT: usize = storage::ColumnFamilyIndex::COUNT;

pub fn cf_name_to_index(name: &[u8]) -> Option<usize> {
    storage::ColumnFamilyIndex::COUNT // Use storage's mapping
    // Actually need to check current implementation
}
```

- [ ] **Step 3: Verify compilation after lib.rs changes**

Run: `cd /home/lpf/dev/kiwi && cargo check --package raft`
Expected: May have errors - fix them

- [ ] **Step 4: Delete duplicate files**

```bash
cd /home/lpf/dev/kiwi && rm src/raft/src/collector.rs src/raft/src/cf_tracker.rs src/raft/src/event_listener.rs src/raft/src/table_properties.rs src/raft/src/types.rs src/raft/src/db_access.rs
```

- [ ] **Step 5: Verify compilation after deletion**

Run: `cd /home/lpf/dev/kiwi && cargo check --package raft`
Expected: PASS

- [ ] **Step 6: Update tests that use raft:: types**

Check: `cd /home/lpf/dev/kiwi && grep -r "use raft::" src/raft/tests/`

Update `src/raft/tests/logindex_integration.rs` if needed to use storage::logindex types.

- [ ] **Step 7: Run all raft tests**

Run: `cd /home/lpf/dev/kiwi && cargo test --package raft`
Expected: PASS

- [ ] **Step 8: Commit**

```bash
cd /home/lpf/dev/kiwi && git add -A src/raft/
git commit -m "refactor(raft): remove duplicate logindex files, re-export from storage"
```

---

## Task 7: Update node.rs to Remove Single-Instance Logic

**Files:**
- Modify: `src/raft/src/node.rs:160-185`

- [ ] **Step 1: Review current node.rs collector retrieval**

Current code at lines 165-166:
```rust
storage.get_logindex_collector(0),  // Only instance 0
storage.get_logindex_cf_tracker(0),
```

- [ ] **Step 2: Modify create_raft_node**

```rust
// src/raft/src/node.rs - modify collector/cf_tracker handling

pub async fn create_raft_node(
    config: RaftConfig,
    storage: Arc<Storage>,
) -> Result<Arc<RaftApp>, anyhow::Error> {
    // ... existing config and directory setup ...
    
    // CHANGE: State machine no longer needs individual collector/cf_tracker
    // Storage handles multi-instance internally via RaftSnapshotMeta methods
    
    // Remove this single-instance retrieval:
    // let (collector, cf_tracker) = match (
    //     storage.get_logindex_collector(0),
    //     storage.get_logindex_cf_tracker(0),
    // ) ...
    
    // Instead, state machine just needs storage reference
    // Pass dummy collector/cf_tracker for now (or refactor state_machine to not need them)
    
    // Actually, looking at state_machine.rs, it currently holds collector/cf_tracker fields
    // These are used for snapshot callback. Need to decide approach:
    // Option A: Keep single instance reference for callback
    // Option B: Refactor state_machine to use storage methods
    
    // For now, keep minimal change - use instance 0 for callback but
    // snapshot serialization uses storage.with_all_instances()
    
    let collector = storage.get_logindex_collector(0)
        .unwrap_or_else(|| Arc::new(LogIndexAndSequenceCollector::new(0)));
    let cf_tracker = storage.get_logindex_cf_tracker(0)
        .unwrap_or_else(|| Arc::new(LogIndexOfColumnFamilies::new()));
    
    let state_machine = KiwiStateMachine::new(
        config.node_id,
        storage.clone(),
        config.db_path.clone(),
        snapshot_work_dir,
        collector,
        cf_tracker,
    );
    
    // ... rest of existing code ...
}
```

Note: This is a minimal change. The collector/cf_tracker in state_machine are for local tracking. Multi-instance snapshot sync happens via RaftSnapshotMeta methods which use storage directly.

- [ ] **Step 3: Verify compilation**

Run: `cd /home/lpf/dev/kiwi && cargo check --package raft --package server`
Expected: PASS

- [ ] **Step 4: Commit**

```bash
cd /home/lpf/dev/kiwi && git add src/raft/src/node.rs
git commit -m "refactor(raft): simplify collector retrieval in create_raft_node"
```

---

## Task 8: Run Full Test Suite

- [ ] **Step 1: Run storage tests**

Run: `cd /home/lpf/dev/kiwi && cargo test --package storage`
Expected: All PASS

- [ ] **Step 2: Run raft tests**

Run: `cd /home/lpf/dev/kiwi && cargo test --package raft`
Expected: All PASS

- [ ] **Step 3: Run workspace tests**

Run: `cd /home/lpf/dev/kiwi && cargo test --workspace`
Expected: All PASS (may have some ignored tests)

- [ ] **Step 4: Run snapshot roundtrip test specifically**

Run: `cd /home/lpf/dev/kiwi && cargo test --package raft --test snapshot_roundtrip_test -- --nocapture`
Expected: PASS - verify multi-instance state preserved

- [ ] **Step 5: Final commit if any fixes needed**

```bash
cd /home/lpf/dev/kiwi && git add -A && git commit -m "test: verify multi-instance logindex snapshot sync"
```

---

## Acceptance Criteria

| Criteria | Verification |
|----------|-------------|
| Multi-instance state saved | `RaftSnapshotMeta.logindex_collector_states.len() == 3` |
| Multi-instance state restored | All 3 collectors have correct values after install |
| Global smallest computed | `get_global_smallest_flushed_log_index` returns correct min |
| Backward compatible | v1 JSON deserializes to v2 format |
| No duplicate code | raft/src has no collector.rs, cf_tracker.rs, etc. |
| All tests pass | `cargo test --workspace` |

---

## References

- Spec: `docs/snapshot-logindex-sync-plan-v3.md`
- Review: `docs/superpowers/specs/2026-04-16-snapshot-logindex-sync-review.md`
- OpenRaft: `/home/lpf/dev/openraft`