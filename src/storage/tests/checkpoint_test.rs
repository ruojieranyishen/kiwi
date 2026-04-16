// Copyright (c) 2024-present, arana-db Community.  All rights reserved.
//
// Licensed to the Apache Software Foundation (ASF) under one or more
// contributor license agreements.  See the NOTICE file distributed with
// this work for additional information regarding copyright ownership.
// The ASF licenses this file to You under the Apache License, Version 2.0
// (the "License"); you may not use this file except in compliance with
// the License.  You may obtain a copy of the License at
//
//   http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

// Copyright (c) 2024-present, arana-db Community.  All rights reserved.

#![allow(clippy::unwrap_used)]

use std::sync::Arc;

use storage::{
    RaftSnapshotMeta, StorageOptions, restore_checkpoint_layout, storage::Storage,
    unique_test_db_path,
};

#[tokio::test]
async fn l1_checkpoint_roundtrip() {
    let db_path = unique_test_db_path();
    let cp_root = unique_test_db_path();

    let mut storage = Storage::new(1, 0);
    let options = Arc::new(StorageOptions::default());
    let _rx = storage.open(options.clone(), &db_path).unwrap();

    storage.set(b"k_l1", b"v1").unwrap();

    let meta = RaftSnapshotMeta::new(42, 7);
    storage.create_checkpoint(&cp_root, &meta).unwrap();
    let read_back = RaftSnapshotMeta::read_from_dir(&cp_root).unwrap();
    assert_eq!(read_back, meta);

    let restore_path = unique_test_db_path();
    restore_checkpoint_layout(&cp_root, &restore_path, 1).unwrap();

    let mut storage2 = Storage::new(1, 0);
    let _rx2 = storage2.open(options, &restore_path).unwrap();
    assert_eq!(storage2.get(b"k_l1").unwrap(), "v1");
}

/// Restoring from a checkpoint should materialize the captured state in a fresh path,
/// even if the source storage has changed afterwards.
#[tokio::test]
async fn restore_checkpoint_to_new_storage_after_source_mutation() {
    let db_path = unique_test_db_path();
    let cp_root = unique_test_db_path();
    let restore_path = unique_test_db_path();

    let mut storage = Storage::new(1, 0);
    let options = Arc::new(StorageOptions::default());
    let _rx = storage.open(options.clone(), &db_path).unwrap();

    storage.set(b"k_rep", b"from_cp").unwrap();

    let meta = RaftSnapshotMeta::new(1, 1);
    storage.create_checkpoint(&cp_root, &meta).unwrap();

    storage.set(b"k_rep", b"after_cp").unwrap();

    restore_checkpoint_layout(&cp_root, &restore_path, 1).unwrap();

    let mut restored = Storage::new(1, 0);
    let _rx2 = restored.open(options, &restore_path).unwrap();
    assert_eq!(restored.get(b"k_rep").unwrap(), "from_cp");
}

/// Test that snapshot metadata includes version field for future compatibility.
#[test]
fn test_snapshot_meta_version() {
    let meta = RaftSnapshotMeta::new(100, 5);

    // Verify version is serialized (now version 2 for multi-instance support)
    let json = serde_json::to_string(&meta).unwrap();
    assert!(json.contains("\"version\":2"));

    // Verify version is deserialized correctly
    let deserialized: RaftSnapshotMeta = serde_json::from_str(&json).unwrap();
    assert_eq!(deserialized.version, 2);
    assert_eq!(deserialized.last_included_index, 100);
    assert_eq!(deserialized.last_included_term, 5);
}

/// Test that reading a snapshot with missing version field fails (no backward compatibility).
#[test]
fn snapshot_meta_rejects_missing_version() {
    // Old format JSON without version field
    let old_format_json = r#"{
        "last_included_index": 42,
        "last_included_term": 7
    }"#;

    let result: Result<RaftSnapshotMeta, _> = serde_json::from_str(old_format_json);
    assert!(result.is_err(), "Should reject old format without version field");
}

/// Test version validation - rejects version 0.
#[test]
fn test_snapshot_meta_rejects_version_zero() {
    use std::fs;

    let tmp_dir = tempfile::tempdir().unwrap();
    let meta_path = tmp_dir.path().join("__raft_snapshot_meta");

    // Write invalid version 0 to file
    let invalid_json = r#"{
        "version": 0,
        "last_included_index": 42,
        "last_included_term": 7
    }"#;
    fs::write(&meta_path, invalid_json).unwrap();

    let result = RaftSnapshotMeta::read_from_dir(tmp_dir.path());
    assert!(
        result.is_err(),
        "Should reject snapshot with version 0"
    );
}

/// Test that higher version is accepted (forward compatibility).
#[test]
fn test_snapshot_meta_accepts_higher_version() {
    use std::fs;

    let tmp_dir = tempfile::tempdir().unwrap();
    let meta_path = tmp_dir.path().join("__raft_snapshot_meta");

    // Write future version to file
    let future_json = r#"{
        "version": 999,
        "last_included_index": 42,
        "last_included_term": 7
    }"#;
    fs::write(&meta_path, future_json).unwrap();

    let result = RaftSnapshotMeta::read_from_dir(tmp_dir.path());
    assert!(
        result.is_ok(),
        "Should accept higher versions for forward compatibility"
    );
    assert_eq!(result.unwrap().version, 999);
}

/// Test version boundary at u32 max.
#[test]
fn test_snapshot_meta_max_version() {
    let meta = RaftSnapshotMeta {
        version: u32::MAX,
        last_included_index: 42,
        last_included_term: 7,
        logindex_collector_states: Vec::new(),
    };

    let json = serde_json::to_string(&meta).unwrap();
    let deserialized: RaftSnapshotMeta = serde_json::from_str(&json).unwrap();
    assert_eq!(deserialized.version, u32::MAX);
}

/// Test that RaftSnapshotMeta supports multi-instance logindex collector state.
#[test]
fn test_raft_snapshot_meta_multi_instance_format() {
    // Test that new format can serialize/deserialize multiple instance states
    let meta = RaftSnapshotMeta {
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
    let parsed: RaftSnapshotMeta = serde_json::from_str(&json).unwrap();

    assert_eq!(parsed.logindex_collector_states.len(), 3);
    assert_eq!(parsed.logindex_collector_states[0], vec!["100:1000"]);
}

/// Test backward compatibility: v1 format (single instance) can be read with new code.
/// Note: serde alias handles field name mapping, but type conversion (Vec<String> -> Vec<Vec<String>>)
/// requires custom deserialization logic. This test verifies that the field name alias works,
/// but the actual v1->v2 data migration should be handled separately.
#[test]
fn test_raft_snapshot_meta_backward_compatibility_field_alias() {
    // New v2 format with pluralized field name should work
    let v2_format_json = r#"{
        "version": 2,
        "last_included_index": 100,
        "last_included_term": 5,
        "logindex_collector_states": [["50:500", "60:600"]]
    }"#;

    let parsed: RaftSnapshotMeta = serde_json::from_str(v2_format_json).unwrap();
    assert_eq!(parsed.logindex_collector_states.len(), 1);
    assert_eq!(parsed.logindex_collector_states[0], vec!["50:500", "60:600"]);

    // Alias field name works for identical types (v2 format with old field name)
    let aliased_json = r#"{
        "version": 2,
        "last_included_index": 100,
        "last_included_term": 5,
        "logindex_collector_state": [["50:500", "60:600"]]
    }"#;

    let parsed_alias: RaftSnapshotMeta = serde_json::from_str(aliased_json).unwrap();
    assert_eq!(parsed_alias.logindex_collector_states.len(), 1);
    assert_eq!(parsed_alias.logindex_collector_states[0], vec!["50:500", "60:600"]);
}

/// Test that missing collector state field defaults to empty vec.
#[test]
fn test_raft_snapshot_meta_defaults_empty_states() {
    let json = r#"{
        "version": 2,
        "last_included_index": 100,
        "last_included_term": 5
    }"#;

    let parsed: RaftSnapshotMeta = serde_json::from_str(json).unwrap();
    assert_eq!(parsed.logindex_collector_states.len(), 0);
}

/// Test with_all_instances aggregates collector states from multiple instances
#[tokio::test]
async fn test_with_all_instances_aggregates_multiple_collectors() {
    let db_path = tempfile::tempdir().unwrap().path().to_path_buf();
    let mut storage = Storage::new(3, 0);
    let options = Arc::new(StorageOptions::default());
    let _rx = storage.open(options, &db_path).unwrap();

    // Update each instance's collector with different log_index
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
    let meta = RaftSnapshotMeta::with_all_instances(300, 1, &storage);

    // Verify all 3 instance states captured
    assert_eq!(meta.logindex_collector_states.len(), 3);
    // Each instance should have one entry
    assert!(!meta.logindex_collector_states[0].is_empty());
    assert!(!meta.logindex_collector_states[1].is_empty());
    assert!(!meta.logindex_collector_states[2].is_empty());
}

/// Test restore_to_storage restores all instances
#[tokio::test]
async fn test_restore_to_storage_restores_all_instances() {
    let db_path = tempfile::tempdir().unwrap().path().to_path_buf();
    let mut storage = Storage::new(3, 0);
    let options = Arc::new(StorageOptions::default());
    let _rx = storage.open(options, &db_path).unwrap();

    // Create meta with states for all 3 instances
    let meta = RaftSnapshotMeta {
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

/// Test backward compatibility: v1 format (single Vec<String>) should be migratable via from_v1
#[test]
fn test_from_v1_backward_compatibility() {
    // Create v1-style single instance state
    let single_state = vec!["100:1000".to_string(), "200:2000".to_string()];

    // Convert to v2 format using from_v1
    let meta = RaftSnapshotMeta::from_v1(200, 1, single_state);

    // Verify conversion
    assert_eq!(meta.version, 2);
    assert_eq!(meta.last_included_index, 200);
    assert_eq!(meta.last_included_term, 1);
    assert_eq!(meta.logindex_collector_states.len(), 1);
    assert_eq!(meta.logindex_collector_states[0].len(), 2);
    assert_eq!(meta.logindex_collector_states[0][0], "100:1000");
    assert_eq!(meta.logindex_collector_states[0][1], "200:2000");
}

/// Test that v1 JSON can be read and converted via read_v1_from_dir
#[test]
fn test_read_v1_snapshot() {
    use std::fs;

    let tmp_dir = tempfile::tempdir().unwrap();
    let meta_path = tmp_dir.path().join("__raft_snapshot_meta");

    // Write v1 format JSON (single instance state as flat Vec<String>)
    let v1_json = r#"{
        "version": 1,
        "last_included_index": 200,
        "last_included_term": 1,
        "logindex_collector_state": ["100:1000", "200:2000"]
    }"#;
    fs::write(&meta_path, v1_json).unwrap();

    // read_from_dir should now handle v1 gracefully via read_v1_from_dir
    let result = RaftSnapshotMeta::read_from_dir(tmp_dir.path());
    assert!(result.is_ok(), "Should successfully read v1 format snapshot");

    let meta = result.unwrap();
    assert_eq!(meta.version, 2); // Converted to v2 internally
    assert_eq!(meta.last_included_index, 200);
    assert_eq!(meta.last_included_term, 1);
    assert_eq!(meta.logindex_collector_states.len(), 1);
    assert_eq!(meta.logindex_collector_states[0].len(), 2);
}
