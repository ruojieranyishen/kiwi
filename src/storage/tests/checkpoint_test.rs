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

    let meta = RaftSnapshotMeta {
        last_included_index: 42,
        last_included_term: 7,
    };
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

    let meta = RaftSnapshotMeta {
        last_included_index: 1,
        last_included_term: 1,
    };
    storage.create_checkpoint(&cp_root, &meta).unwrap();

    storage.set(b"k_rep", b"after_cp").unwrap();

    restore_checkpoint_layout(&cp_root, &restore_path, 1).unwrap();

    let mut restored = Storage::new(1, 0);
    let _rx2 = restored.open(options, &restore_path).unwrap();
    assert_eq!(restored.get(b"k_rep").unwrap(), "from_cp");
}
