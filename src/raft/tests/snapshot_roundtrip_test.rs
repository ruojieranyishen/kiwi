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

use openraft::RaftSnapshotBuilder;
use openraft::storage::RaftStateMachine;
use raft::state_machine::KiwiStateMachine;
use storage::unique_test_db_path;
use storage::{StorageOptions, storage::Storage};

#[tokio::test]
async fn cursor_snapshot_roundtrip() -> anyhow::Result<()> {
    let src_db_path = unique_test_db_path();
    let restore_db_path = unique_test_db_path();
    let snap_root = unique_test_db_path();

    std::fs::create_dir_all(&snap_root)?;

    let storage = {
        let mut s = Storage::new(1, 0);
        let options = Arc::new(StorageOptions::default());
        let _rx = s.open(options, &src_db_path)?;
        Arc::new(s)
    };

    storage.set(b"k_l2", b"before")?;

    let mut sm = KiwiStateMachine::new(
        1,
        Arc::clone(&storage),
        src_db_path.clone(),
        snap_root.clone(),
    );

    let mut builder = sm.get_snapshot_builder().await;
    let snap = builder.build_snapshot().await?;
    assert!(!snap.snapshot.get_ref().is_empty());

    let cur = sm
        .get_current_snapshot()
        .await?
        .expect("OpenRaft requires current snapshot after build");
    assert_eq!(cur.meta, snap.meta);

    storage.set(b"k_l2", b"after")?;
    assert_eq!(storage.get(b"k_l2")?, "after");

    let meta = snap.meta.clone();
    let bytes = snap.snapshot.into_inner();
    // Create target storage but do NOT open it yet - this is expected because
    // install_snapshot will restore the checkpoint directly to db_path, bypassing
    // the normal open flow. The storage is opened after install_snapshot completes.
    let target_storage = Arc::new(Storage::new(1, 0));
    let mut sm2 = KiwiStateMachine::new(2, target_storage, restore_db_path.clone(), snap_root);
    sm2.install_snapshot(&meta, Box::new(std::io::Cursor::new(bytes)))
        .await?;

    let cur2 = sm2
        .get_current_snapshot()
        .await?
        .expect("OpenRaft requires current snapshot after install");
    assert_eq!(cur2.meta, meta);

    let mut restored = Storage::new(1, 0);
    let options = Arc::new(StorageOptions::default());
    let _rx = restored.open(options, &restore_db_path)?;
    assert_eq!(restored.get(b"k_l2")?, "before");

    Ok(())
}
