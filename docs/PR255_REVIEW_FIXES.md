# PR #255 Review 意见修改方案

## PR 概述

**标题**: `feat(raft): implement snapshot creation and restore`

**功能**: 实现 Raft 快照的核心功能，包括 checkpoint 创建、tar 打包、解包恢复的完整流程。

---

## 用户反馈汇总

根据用户反馈，review 意见的修复优先级调整如下：

| 问题 | 描述 | 用户反馈 |
|------|------|----------|
| 1 | 数据损坏风险 (install_snapshot 后实例句柄失效) | **待确认** - 分片迁移场景下目标节点是否有旧数据 |
| 2 | TOCTOU 竞态条件 | **可以修复** |
| 3 | 路径遍历安全漏洞 | **可以修复** |
| 4 | 流式快照 (内存安全) | **暂不修复** - 先跑起来，后续优化 |
| 5 | 错误处理 (临时目录清理) | 可以修复 |

**调用场景澄清**:
- **分片迁移或扩展**：源节点将自身数据打包为 checkpoint，目标节点应用这个 checkpoint，然后通过主 WAL 日志追上
- 参考 OpenRaft：https://github.com/databendlabs/openraft

---

## Review 意见汇总与修改方案

### [P0] 数据损坏风险 - install_snapshot 后 Redis 实例持有旧文件句柄

**问题描述**:
`restore_checkpoint_layout` 替换 `db_path` 目录后，`self.storage.insts` 中的 Redis 实例仍然持有**旧的文件句柄**，指向已被删除的旧数据文件。

**架构分析**:
```
build_snapshot (源节点):                install_snapshot (目标节点):
┌─────────────────────────┐            ┌─────────────────────────┐
│  Storage.insts[0..n]    │            │  Storage.insts[0..n]    │
│  (RocksDB 实例打开中)     │            │  (RocksDB 实例打开中)     │
│         ↓               │            │         ↓               │
│  create_checkpoint()    │            │  restore_checkpoint()   │
│  (RocksDB 原生 API)       │            │  (替换 db_path 目录)      │
│         ↓               │            │         ↓               │
│  生成 checkpoint/<i>/    │            │  db_path 被新数据替换     │
│         ↓               │            │  ❓ insts 仍指向旧文件？  │
│  tar 打包 → 发送         │            │  → 通过 WAL 日志追上      │
└─────────────────────────┘            └─────────────────────────┘
```

**使用场景** (用户澄清):
- **分片迁移或扩展**：源节点将自身数据打包为 checkpoint，目标节点应用这个 checkpoint，然后通过主 WAL 日志追上
- 参考 OpenRaft 的 snapshot 使用方式：https://github.com/databendlabs/openraft

**关键问题**:
- 如果目标节点是**空实例**（新节点加入），则问题不存在
- 如果目标节点已有**旧数据**（分片再平衡），则需要重新打开实例

**用户反馈**:
- 调用场景：分片迁移或扩展
- 流程：主副本打包 checkpoints → 目标节点应用 checkpoint → 通过主 WAL 日志追上
- 参考：OpenRaft 的使用方式 (https://github.com/databendlabs/openraft)

**待确认**:
- 分片迁移时，目标节点是否已有旧数据？
  - 如果是空实例 → 不需要修复
  - 如果有旧数据 → 需要 `reload_instances()` 重新打开

**补充说明**:

根据用户说明：
> 分片迁移时目标节点若存在旧 checkpoints 需要先删除，不会往相同实例已打开的 checkpoints 上去同步数据

这意味着：
1. 目标节点在 `install_snapshot` 之前，应该已经删除了旧的 checkpoint 数据
2. 需要添加**断言**确保目标节点是空实例或旧数据已被清理
3. 不应该出现往已打开的 RocksDB 实例上覆盖数据的情况

**修改方案**:

```rust
// src/raft/src/state_machine.rs
async fn install_snapshot(&mut self, ...) -> Result<(), StorageError<u64>> {
    // 添加断言：确保目标节点没有旧数据
    // 或者确保旧实例已被正确关闭
    debug_assert!(
        !self.db_path.exists() || is_empty_dir(&self.db_path),
        "install_snapshot: db_path should be empty before restoring checkpoint"
    );
    
    // ... 解压和恢复逻辑 ...
    restore_checkpoint_layout(...)?;
    
    // 如果需要重新打开实例（目标节点有旧数据的情况）
    // let mut_storage = Arc::get_mut(&mut self.storage)...;
    // mut_storage.reload_instances(options, &self.db_path)?;
    
    Ok(())
}
```

**优先级**: **待确认** - 需要添加断言确保目标节点状态正确

**优先级**: P0 (待确认场景后决定是否需要修复)

---

### [P0] TOCTOU 竞态条件 - persist_current_snapshot 和 load_current_snapshot 缺少原子性保护

**问题描述**:
文件操作在检查和写入之间没有原子性保护，文件可能在检查和写入之间被删除/修改。

**修改方案**:

使用临时文件 + 原子重命名模式:

```rust
// src/raft/src/state_machine.rs
fn persist_current_snapshot(
    work_dir: &std::path::Path,
    meta: &SnapshotMeta<u64, KiwiNode>,
    bytes: &[u8],
) -> Result<(), StorageError<u64>> {
    std::fs::create_dir_all(work_dir).map_err(|e| {
        StorageError::from_io_error(ErrorSubject::Snapshot(None), ErrorVerb::Write, e)
    })?;
    
    let data_path = work_dir.join(CURRENT_SNAPSHOT_DATA);
    let meta_path = work_dir.join(CURRENT_SNAPSHOT_META);
    
    // 写入临时文件
    let data_tmp = work_dir.join(format!(".{}.tmp", CURRENT_SNAPSHOT_DATA));
    let meta_tmp = work_dir.join(format!(".{}.tmp", CURRENT_SNAPSHOT_META));
    
    std::fs::write(&data_tmp, bytes).map_err(|e| {
        StorageError::from_io_error(ErrorSubject::Snapshot(None), ErrorVerb::Write, e)
    })?;
    
    let json = serde_json::to_string_pretty(meta).map_err(|e| {
        StorageError::from_io_error(
            ErrorSubject::Snapshot(None),
            ErrorVerb::Write,
            io::Error::other(e.to_string()),
        )
    })?;
    std::fs::write(&meta_tmp, json).map_err(|e| {
        StorageError::from_io_error(ErrorSubject::Snapshot(None), ErrorVerb::Write, e)
    })?;
    
    // 原子重命名 (POSIX 系统上，同文件系统内的 rename 是原子的)
    std::fs::rename(&data_tmp, &data_path).map_err(|e| {
        StorageError::from_io_error(ErrorSubject::Snapshot(None), ErrorVerb::Write, e)
    })?;
    std::fs::rename(&meta_tmp, &meta_path).map_err(|e| {
        StorageError::from_io_error(ErrorSubject::Snapshot(None), ErrorVerb::Write, e)
    })?;
    
    Ok(())
}
```

**优先级**: P0 (阻塞性) - **用户确认：可以修复**

---

### [P0] 路径遍历风险 - unpack_tar_to_dir 没有安全检查

**问题描述**:
`unpack_tar_to_dir` 对解压路径没有安全检查，恶意 tar 可能包含 `../../../etc/passwd`。

**修改方案**:

```rust
// src/raft/src/snapshot_archive.rs
pub fn unpack_tar_to_dir(bytes: &[u8], dst: &Path) -> io::Result<()> {
    // 1. 验证 dst 是目录
    if dst.exists() && !dst.is_dir() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "destination is not a directory",
        ));
    }
    
    std::fs::create_dir_all(dst)?;
    
    let mut archive = tar::Archive::new(Cursor::new(bytes));
    
    // 2. 验证每个条目路径
    for entry in archive.entries()? {
        let mut entry = entry?;
        let path = entry.path()?.to_path_buf();
        
        // 检查路径遍历攻击
        if path.components().any(|c| c.as_os_str() == "..") {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("malicious path detected: {:?}", path),
            ));
        }
        
        // 确保路径在 dst 内
        let full_path = dst.join(path);
        if !full_path.starts_with(dst) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "path escapes destination",
            ));
        }
    }
    
    // 3. 安全解压
    archive.unpack(dst)?;
    Ok(())
}
```

**优先级**: P0 (阻塞性) - **用户确认：可以修复**

---

### [P1] 缺少错误处理 - 快照构建失败不清理临时目录

**问题描述**:
`let _ = std::fs::remove_dir_all(&dir)` 忽略清理错误。

**修改方案**:

```rust
// src/raft/src/state_machine.rs - KiwiSnapshotBuilder::build_snapshot
impl RaftSnapshotBuilder<KiwiTypeConfig> for KiwiSnapshotBuilder {
    async fn build_snapshot(&mut self) -> Result<Snapshot<KiwiTypeConfig>, StorageError<u64>> {
        let dir = self.snapshot_work_dir.join(format!("build-{}", self._idx));
        
        // 使用 RAII 模式确保清理
        let _cleanup_guard = scopeguard::guard(dir.clone(), |d| {
            if let Err(e) = std::fs::remove_dir_all(&d) {
                log::error!("Failed to cleanup snapshot temp dir {:?}: {}", d, e);
            }
        });
        
        if dir.exists() {
            std::fs::remove_dir_all(&dir).map_err(io_err_to_raft)?;
        }
        std::fs::create_dir_all(&dir).map_err(io_err_to_raft)?;
        
        // ... 构建快照 ...
        
        let bytes = pack_dir_to_vec(&dir).map_err(io_err_to_raft)?;
        
        // 显式清理，成功后取消 guard
        std::fs::remove_dir_all(&dir).map_err(io_err_to_raft)?;
        scopeguard::ScopeGuard::into_inner(_cleanup_guard);
        
        Ok(Snapshot { ... })
    }
}
```

或者使用 `tempfile` crate 的自动清理:

```rust
let temp_dir = tempfile::tempdir().map_err(io_err_to_raft)?;
// temp_dir 在 drop 时自动清理
```

**优先级**: P1

---

### [P1] 内存安全 - pack_dir_to_vec 将整个 tar 加载到内存

**问题描述**:
对于 GB 级数据库，将整个 tar 加载到内存会导致 OOM。

**当前状态**:
代码中已有 TODO 说明：
```rust
/// TODO: stream snapshot bytes (read/write without holding the full tar in memory)
/// for build and install paths; align with OpenRaft snapshot APIs
```

**用户反馈**: 暂不修复 - 当前实现适用于中小规模数据库，流式快照可作为后续优化

**长期方案** (可选后续 PR):
使用流式 API 替代内存缓冲区，参考 OpenRaft Snapshot 流式接口。

**优先级**: P1 - **用户确认：暂不修复**

---

### [P1] 竞态条件 - restore_checkpoint_layout 非原子操作

**问题描述**:
删除和重命名之间存在时间窗口，进程崩溃会导致数据完全丢失。

**当前状态**:
查看 `checkpoint.rs` 代码，已经实现了原子替换模式:
```rust
pub fn restore_checkpoint_layout(...) -> io::Result<()> {
    // 1. 验证源目录
    for i in 0..db_instance_num { ... }
    
    // 2. 复制到临时目录
    let temp_dir = target_db_path.with_file_name(format!(".restore_temp_{}", std::process::id()));
    // ... 复制到 temp_dir ...
    
    // 3. 原子交换
    if target_db_path.exists() {
        fs::remove_dir_all(target_db_path)?;
    }
    fs::rename(&temp_dir, target_db_path)?;  // 原子操作
    
    Ok(())
}
```

**建议改进** - 添加崩溃恢复:
```rust
// 在启动时检查并清理残留的临时目录
pub fn cleanup_restore_temp_dirs(db_path: &Path) -> io::Result<()> {
    if let Some(parent) = db_path.parent() {
        for entry in fs::read_dir(parent)? {
            let entry = entry?;
            let name = entry.file_name();
            if name.to_string_lossy().starts_with(".restore_temp_") {
                fs::remove_dir_all(entry.path())?;
            }
        }
    }
    Ok(())
}
```

**优先级**: P1 (已部分解决，建议添加崩溃恢复)

---

### [P1] 测试覆盖不足

**问题描述**:
测试只覆盖正常路径，缺少:
- 损坏数据测试
- 空数据测试
- 大文件测试
- 并发访问测试

**修改方案**:

```rust
// src/raft/tests/snapshot_tests.rs

/// 测试损坏的快照元数据
#[test]
fn test_corrupted_snapshot_meta() {
    // 1. 创建损坏的 meta 文件
    // 2. 验证 load_current_snapshot 返回错误而不是 panic
}

/// 测试空数据库快照
#[test]
fn test_empty_database_snapshot() {
    // 1. 创建空数据库
    // 2. 构建快照
    // 3. 安装快照
    // 4. 验证空数据库正确恢复
}

/// 测试大文件快照 (可选，耗时)
#[test]
#[ignore]
fn test_large_snapshot() {
    // 1. 创建大量数据
    // 2. 构建快照
    // 3. 验证内存使用在合理范围内
}

/// 测试并发快照操作
#[tokio::test]
async fn test_concurrent_snapshot_operations() {
    // 1. 并发执行 build_snapshot 和 install_snapshot
    // 2. 验证没有数据竞争
}

/// 测试路径遍历攻击防护
#[test]
fn test_path_traversal_protection() {
    // 1. 创建恶意 tar 包含 ../ 路径
    // 2. 验证 unpack_tar_to_dir 拒绝解压
}

/// 测试 TOCTOU 防护
#[test]
fn test_toctou_protection() {
    // 1. 模拟并发写入同一快照文件
    // 2. 验证原子性
}
```

**优先级**: P1

---

### [P2] 未使用字段

**问题描述**:
`KiwiSnapshotBuilder` 中 `_node_id`, `_storage`, `_idx` 字段未使用。

**当前状态**:
```rust
pub struct KiwiSnapshotBuilder {
    _storage: Arc<Storage>,  // 用于 create_checkpoint
    _idx: u64,               // 用于 snapshot_id 和临时目录名
    snapshot_work_dir: PathBuf,
    last_applied: Option<LogId<u64>>,
    last_membership: StoredMembership<u64, KiwiNode>,
    _node_id: u64,           // TODO: 需要时使用
}
```

**分析**:
- `_storage`: 实际上在 `build_snapshot` 中使用 `self._storage.create_checkpoint(...)`
- `_idx`: 用于 `format!("build-{}", self._idx)` 和 `format!("snapshot-{}", self._idx)`
- `_node_id`: 当前未使用，但在 `last_applied` 为 None 时可能需要用于创建初始 leader_id

**建议**:
保留这些字段，移除 `_` 前缀或添加 `#[allow(dead_code)]` 注释说明未来用途。

**优先级**: P2 (低)

---

### [P2] 快照版本兼容性

**建议**:
为快照元数据添加版本号，以便未来格式变更时保持向后兼容。

**修改方案**:

```rust
// src/storage/src/checkpoint.rs
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RaftSnapshotMeta {
    pub version: u32,  // 新增版本字段
    pub last_included_index: u64,
    pub last_included_term: u64,
}

impl RaftSnapshotMeta {
    pub fn new(index: u64, term: u64) -> Self {
        Self {
            version: 1,
            last_included_index: index,
            last_included_term: term,
        }
    }
}
```

**优先级**: P2 (建议添加)

---

### CodeRabbit 评论

#### 1. snapshot_archive.rs - 验证目标路径类型

**问题**: `unpack_tar_to_dir` 在 `dst.exists()` 时调用 `remove_dir_all` 前未验证 `dst` 是目录。

**修复**: 已在上述 [P0] 路径遍历风险中涵盖。

#### 2. state_machine.rs - 临时目录错误路径清理

**问题**: 临时目录可能在错误路径上未显式清理。

**修复**: 已在上述 [P1] 缺少错误处理中涵盖，建议使用 `scopeguard` 或 `tempfile`。

#### 3. state_machine.rs - 快照元数据 index/term 不一致

**问题**: 快照元数据可能混合使用 storage 的 index 和 `last_applied` 的 term，产生不一致的 LogId。

**当前状态分析**:
```rust
let (last_idx, last_term) = if let Some(last_log_id) = self.last_applied {
    (last_log_id.index, last_log_id.leader_id.term)  // 正确：来自同一 LogId
} else {
    (0, 0)
};
```

**分析**: 当前代码是正确的，`(index, term)` 来自同一 `LogId`。需要确认没有其他地方混合使用不同来源的 index 和 term。

**建议**: 添加注释说明:
```rust
// IMPORTANT: (index, term) must come from the same LogId to maintain
// Raft invariant. Never mix index from one source with term from another.
```

#### 4. redis.rs - smallest_flushed_log_index 返回 None

**问题**: `smallest_flushed_log_index` 必须从 log store 读取持久化的刷新日志边界。

**修改方案**:
```rust
// src/storage/src/redis.rs (或 storage.rs)
impl Storage {
    pub fn smallest_flushed_log_index(&self) -> Result<u64> {
        // TODO: Read from persisted log store
        // For now, return 0 as placeholder
        // This needs to be implemented when log store persistence is added
        Ok(0)
    }
}
```

**优先级**: P1 (需要实现)

---

## 修改计划

### 立即修复 (本次 PR)

1. **[P0] TOCTOU 竞态** - 使用临时文件 + 原子重命名 ✅
2. **[P0] 路径遍历** - 添加 tar 解压安全检查 ✅
3. **[P1] 错误处理** - 使用 `scopeguard` 或 `tempfile` 确保临时目录清理 ✅

### 待确认问题

4. **[P0] 数据损坏风险** - 分片迁移时目标节点是否有旧数据？
   
   **OpenRaft 参考分析**:
   
   根据 OpenRaft 源码 (`openraft/src/engine/handler/following_handler/mod.rs:244`):
   ```rust
   pub(crate) fn install_full_snapshot(&mut self, snapshot: SnapshotOf<C>) -> Option<Condition<C>> {
       // 检查 snapshot 是否比当前 committed 更新
       if snap_last_log_id.as_ref() <= self.state.committed() {
           return None;  // 不安装更新的 snapshot
       }
       
       // 1. Truncate all logs if conflict
       // 2. Install snapshot
       // 3. Purge logs the snapshot covers
       
       // 如果有冲突，截断所有非 committed logs
       if local != snap_last_log_id {
           self.truncate_logs(self.state.committed().next_index());
       }
       
       // 调用 state machine 的 install_snapshot
       self.output.push_command(Command::from(sm::Command::install_full_snapshot(...)));
   }
   ```
   
   OpenRaft 的 `install_snapshot` 流程：
   1. 检查 snapshot 是否比当前 committed 更新
   2. 如果有日志冲突，先 truncate 冲突的日志
   3. 调用 `RaftStateMachine::install_snapshot` 安装 snapshot
   4. Purge 已应用 log
   
   **示例参考** (`examples/raft-kv-rocksdb/src/store.rs:190`):
   ```rust
   async fn install_snapshot(&mut self, meta: &SnapshotMeta, snapshot: SnapshotData) -> Result<(), io::Error> {
       let new_snapshot = StoredSnapshot {
           meta: meta.clone(),
           data: snapshot.into_inner(),
       };
       
       // 直接更新 state machine 数据
       self.update_state_machine_(new_snapshot.clone()).await?;
       
       // 持久化 snapshot
       self.set_current_snapshot_(new_snapshot)?;
       
       Ok(())
   }
   ```
   
   **关键发现**:
   - OpenRaft 的 `install_snapshot` 假设 state machine 会正确处理数据覆盖
   - 对于内存 state machine (如 BTreeMap)，直接覆盖数据即可
   - 对于 RocksDB 等持久化存储，需要在安装 snapshot 前确保旧数据被正确处理
   
   **建议方案**:
   1. 添加断言确保 `db_path` 在 `install_snapshot` 前是空的或已被清理
   2. 或者在 `install_snapshot` 中显式关闭旧实例再恢复

### 后续优化 (暂不修复)

5. ~~**[P1] 流式快照**~~ - 先跑起来，后续优化
6. **[P2] 未使用字段** - 清理或添加注释 (可选)
7. **[P2] 版本兼容性** - 添加版本号到快照元数据 (可选)
8. ~~**[P1] 竞态条件**~~ - 已部分解决：`restore_checkpoint_layout` 已使用原子替换模式

---

## 测试清单

### 必须测试 (本次 PR)
- [ ] 单元测试：路径遍历攻击防护
- [ ] 单元测试：TOCTOU 防护（临时文件 + 原子重命名）
- [ ] 集成测试：快照创建 → 恢复完整流程
- [ ] **添加断言测试**：确保 `install_snapshot` 前目标节点状态正确

### 可选测试
- [ ] 单元测试：损坏快照元数据处理
- [ ] 单元测试：空数据库快照
- [ ] 集成测试：并发快照操作
- [ ] 压力测试：大数据库快照 (可选，耗时)

---

## 相关文件

- `src/raft/src/state_machine.rs` - 快照构建和安装主逻辑
- `src/raft/src/snapshot_archive.rs` - tar 打包/解包
- `src/storage/src/checkpoint.rs` - checkpoint 布局和恢复
- `src/storage/src/storage.rs` - Storage 实例管理
- `src/storage/src/redis.rs` - Redis 实例管理

---

## 参考

### OpenRaft 源码参考
- **install_snapshot 流程**: `openraft/src/engine/handler/following_handler/mod.rs:install_full_snapshot`
- **示例实现**: `examples/raft-kv-rocksdb/src/store.rs:install_snapshot`
- **State Machine 文档**: `openraft/src/docs/components/state-machine.md`
- **GitHub**: https://github.com/databendlabs/openraft

### Rust 库参考
- Rust tempfile crate: https://docs.rs/tempfile
- Rust scopeguard crate: https://docs.rs/scopeguard

### 项目相关
- PR #255: https://github.com/arana-db/kiwi/pull/255
