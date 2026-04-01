// Copyright (c) 2024-present, arana-db Community.  All rights reserved.
//
// Licensed to the Apache Software Foundation (ASF) under one or more
// contributor license agreements.  See the NOTICE file distributed with
// this work for additional information regarding copyright ownership.
// The ASF licenses this file to You under the Apache License, Version 2.0
// (the "License"); you may not use this file except in compliance with
// the License.  You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

//! Raft snapshot checkpoint layout: one RocksDB checkpoint per DB instance plus `__raft_snapshot_meta`.

use std::fs;
use std::io;
use std::path::Path;

use serde::{Deserialize, Serialize};

/// File name for JSON metadata at the checkpoint root (not OpenRaft's `SnapshotMeta`).
pub const RAFT_SNAPSHOT_META_FILE: &str = "__raft_snapshot_meta";

/// Current snapshot format version
pub const CURRENT_SNAPSHOT_VERSION: u32 = 1;

/// Metadata persisted next to per-instance checkpoint directories.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RaftSnapshotMeta {
    /// Snapshot format version
    pub version: u32,
    /// Last log index included in the snapshot
    pub last_included_index: u64,
    /// Last log term included in the snapshot
    pub last_included_term: u64,
}

impl RaftSnapshotMeta {
    /// Create a new snapshot meta with current version
    pub fn new(last_included_index: u64, last_included_term: u64) -> Self {
        Self {
            version: CURRENT_SNAPSHOT_VERSION,
            last_included_index,
            last_included_term,
        }
    }

    pub fn write_to_dir(&self, dir: &Path) -> io::Result<()> {
        let path = dir.join(RAFT_SNAPSHOT_META_FILE);
        let json = serde_json::to_string_pretty(self)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
        fs::write(path, json)
    }

    /// Write metadata atomically using temp file + rename pattern.
    /// This ensures that the file is either completely written or not present at all.
    pub fn write_to_dir_atomically(&self, dir: &Path) -> io::Result<()> {
        let path = dir.join(RAFT_SNAPSHOT_META_FILE);
        let json = serde_json::to_string_pretty(self)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;

        // Write to a temporary file first, then atomically rename
        let temp_path = dir.join(format!(".{}.tmp", RAFT_SNAPSHOT_META_FILE));
        fs::write(&temp_path, &json)?;

        // Atomic rename (on POSIX systems, rename is atomic if on same filesystem)
        fs::rename(&temp_path, &path)?;

        Ok(())
    }

    pub fn read_from_dir(dir: &Path) -> io::Result<Self> {
        let path = dir.join(RAFT_SNAPSHOT_META_FILE);
        let bytes = fs::read(path)?;
        let meta: Self = serde_json::from_slice(&bytes)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;

        // Validate version
        if meta.version < CURRENT_SNAPSHOT_VERSION {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "unsupported snapshot version: {}, expected >= {}",
                    meta.version, CURRENT_SNAPSHOT_VERSION
                ),
            ));
        }

        Ok(meta)
    }
}

pub fn copy_dir_all(src: &Path, dst: &Path) -> io::Result<()> {
    if !src.is_dir() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("source is not a directory: {}", src.display()),
        ));
    }
    fs::create_dir_all(dst)?;
    for entry in fs::read_dir(src)? {
        let entry = entry?;
        let ty = entry.file_type()?;
        let src_path = entry.path();
        let dst_path = dst.join(entry.file_name());
        if ty.is_dir() {
            copy_dir_all(&src_path, &dst_path)?;
        } else {
            fs::copy(&src_path, &dst_path)?;
        }
    }
    Ok(())
}

/// Copy checkpoint layout from `checkpoint_root` into `target_db_path` (`0/`, `1/`, …).
///
/// Uses atomic replacement pattern to avoid data loss:
/// 1. Validate all source directories exist
/// 2. Copy to a temporary sibling directory
/// 3. Atomically swap by removing old and renaming new
///
/// This ensures that if the snapshot is malformed or copy fails,
/// the original data in target_db_path remains intact.
pub fn restore_checkpoint_layout(
    checkpoint_root: &Path,
    target_db_path: &Path,
    db_instance_num: usize,
) -> io::Result<()> {
    for i in 0..db_instance_num {
        let from = checkpoint_root.join(i.to_string());
        if !from.is_dir() {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                format!("missing checkpoint instance directory: {}", from.display()),
            ));
        }
    }
    let temp_dir = target_db_path.with_file_name(format!(".restore_temp_{}", std::process::id()));

    if temp_dir.exists() {
        fs::remove_dir_all(&temp_dir)?;
    }

    fs::create_dir_all(&temp_dir)?;

    let copy_result = (|| -> io::Result<()> {
        for i in 0..db_instance_num {
            let from = checkpoint_root.join(i.to_string());
            let to = temp_dir.join(i.to_string());
            copy_dir_all(&from, &to)?;
        }
        Ok(())
    })();

    if let Err(e) = copy_result {
        let _ = fs::remove_dir_all(&temp_dir);
        return Err(e);
    }

    if target_db_path.exists() {
        fs::remove_dir_all(target_db_path)?;
    }
    fs::rename(&temp_dir, target_db_path)?;

    Ok(())
}
