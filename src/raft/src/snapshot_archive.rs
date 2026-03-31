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
///
/// # Security
///
/// This function validates tar entries to prevent path traversal attacks.
/// Any entry containing `..` components or escaping the destination directory
/// will be rejected.
pub fn unpack_tar_to_dir(bytes: &[u8], dst: &Path) -> io::Result<()> {
    // 1. Validate dst is a directory if it exists
    if dst.exists() && !dst.is_dir() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("destination is not a directory: {}", dst.display()),
        ));
    }

    std::fs::create_dir_all(dst)?;

    let mut archive = tar::Archive::new(Cursor::new(bytes));

    // 2. Validate all entries before unpacking to prevent path traversal attacks
    for entry in archive.entries()? {
        let entry = entry?;
        let path = entry.path()?.to_path_buf();

        // Check for path traversal attempts
        if path.components().any(|c| c.as_os_str() == "..") {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("malicious path detected (contains '..'): {:?}", path),
            ));
        }

        // Ensure the full path stays within dst
        let full_path = dst.join(&path);
        if !full_path.starts_with(dst) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("path escapes destination: {:?}", path),
            ));
        }
    }

    // 3. Safe to unpack - all entries validated
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

    #[test]
    fn test_unpack_rejects_non_directory() {
        let tmp = tempfile::tempdir().unwrap();
        let file_path = tmp.path().join("not_a_dir");
        std::fs::write(&file_path, b"content").unwrap();

        let result = unpack_tar_to_dir(&[0u8; 10], &file_path);
        assert!(result.is_err());
        assert_eq!(result.unwrap_err().kind(), io::ErrorKind::InvalidInput);
    }

    #[test]
    fn test_unpack_rejects_path_traversal() {
        // Create a malicious tar with path traversal
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("ck");
        std::fs::create_dir_all(&src).unwrap();
        std::fs::write(src.join("normal_file"), b"normal content").unwrap();

        // Create normal tar first
        let bytes = pack_dir_to_vec(&src).unwrap();

        // Manually craft a tar with malicious path
        let mut malicious_tar = Vec::new();
        {
            let mut builder = tar::Builder::new(&mut malicious_tar);
            // Add a file with path traversal attempt
            let mut header = tar::Header::new_gnu();
            header.set_path("../../../etc/passwd").unwrap();
            header.set_size(4);
            header.set_cksum();
            builder.append(&header, b"root").unwrap();
            builder.finish().unwrap();
        }

        let dst = tempfile::tempdir().unwrap();
        let unpack = dst.path().join("u");

        let result = unpack_tar_to_dir(&malicious_tar, &unpack);
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(err.to_string().contains(".."));
    }
}
