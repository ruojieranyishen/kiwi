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

//! Tar packing/unpacking for Raft snapshot checkpoints.
//!
//! TODO: stream snapshot bytes (read/write without holding the full tar in memory) for
//! build and install paths; align with OpenRaft snapshot APIs when switching off in-memory buffers.

use std::io::{self, Cursor};
use std::path::Path;

/// Pack a directory (checkpoint root) into a GNU tar archive in memory.
pub fn pack_dir_to_vec(src: &Path) -> io::Result<Vec<u8>> {
    let mut buf = Vec::new();
    {
        let mut builder = tar::Builder::new(&mut buf);
        builder.append_dir_all("snap", src)?;
        builder.finish()?;
    }
    Ok(buf)
}

/// Unpack a tar archive (from `build_snapshot` / OpenRaft `SnapshotData`) into `dst`.
pub fn unpack_tar_to_dir(bytes: &[u8], dst: &Path) -> io::Result<()> {
    if dst.exists() {
        std::fs::remove_dir_all(dst)?;
    }
    std::fs::create_dir_all(dst)?;
    let mut archive = tar::Archive::new(Cursor::new(bytes));
    archive.unpack(dst)?;
    Ok(())
}

/// Directory inside `unpack_tar_to_dir` output that contains `0/`, `1/`, … and `__raft_snapshot_meta`.
pub fn unpacked_checkpoint_root(unpack_root: &Path) -> std::path::PathBuf {
    unpack_root.join("snap")
}

#[cfg(test)]
mod tests {
    use super::*;
    use storage::RaftSnapshotMeta;

    #[test]
    fn pack_unpack_roundtrip() {
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("ck");
        std::fs::create_dir_all(src.join("0")).unwrap();
        std::fs::write(src.join("0").join("marker"), b"x").unwrap();
        let meta = RaftSnapshotMeta {
            last_included_index: 7,
            last_included_term: 3,
        };
        meta.write_to_dir(&src).unwrap();

        let bytes = pack_dir_to_vec(&src).unwrap();
        assert!(!bytes.is_empty());

        let dst = tempfile::tempdir().unwrap();
        let unpack = dst.path().join("u");
        unpack_tar_to_dir(&bytes, &unpack).unwrap();
        let root = unpacked_checkpoint_root(&unpack);
        assert!(root.join("0").join("marker").exists());
        let m = RaftSnapshotMeta::read_from_dir(&root).unwrap();
        assert_eq!(m.last_included_index, 7);
        assert_eq!(m.last_included_term, 3);
    }
}
