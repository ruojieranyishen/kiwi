# Snapshot LogIndex 同步方案 v3 - 多实例架构完整方案

## 1. 架构背景

### 1.1 多实例分片架构

Kiwi 使用多实例分片架构，数据按 slot 分布到不同的 RocksDB 实例：

```
Storage {
    insts: Vec<Arc<Redis>>   // 默认 3 个实例 (SLOT_INDEXER_INSTANCE_NUM = 3)
}

Redis (实例) {
    db: RocksDB              // 每个实例一个独立的 RocksDB
    // 数据目录: db_path/0, db_path/1, db_path/2
    
    // 每个 RocksDB 有 6 个 Column Family:
    ColumnFamilyIndex {
        MetaCF = 0,         // meta & string
        HashesDataCF = 1,   // hash data
        SetsDataCF = 2,     // set data  
        ListsDataCF = 3,    // list data
        ZsetsDataCF = 4,    // zset data
        ZsetsScoreCF = 5,   // zset score
    }
    
    // LogIndex 状态（每个实例独立）
    logindex_collector: Arc<LogIndexAndSequenceCollector>
    logindex_cf_tracker: Arc<LogIndexOfColumnFamilies>
}
```

### 1.2 数据分布规则

```rust
// storage/src/storage.rs
pub fn on_binlog_write(&self, binlog: &Binlog, raft_log_index: u64) -> Result<()> {
    let slot_id = binlog.slot_idx as usize;
    let instance_id = self.slot_indexer.get_instance_id(slot_id);  // slot_id % instance_num
    let instance = &self.insts[instance_id];
    // ...
}
```

**关键点**：
- 每个实例有独立的 RocksDB → 独立的 sequence number 序列（从 0 开始）
- 同一个 `raft_log_index` 可能对应不同实例的不同 seqno
- 每个实例需要独立的 collector 和 cf_tracker

### 1.3 LogIndex 作用

| 组件 | 作用 | 范围 |
|------|------|------|
| `Collector` | 维护 `(raft_log_index, seqno)` 映射 | 单个实例的单个 RocksDB |
| `CF Tracker` | 追踪各 CF 的 applied/flushed log_index | 单个实例的单个 RocksDB |

**用途**：
1. Flush 完成时，通过 seqno 找到对应的 raft_log_index
2. 更新 cf_tracker 的 flushed_log_index
3. 计算全局 smallest_flushed_log_index（所有实例所有 CF 的最小值）
4. 通知 Raft 可以 purge log_index < smallest_flushed_log_index 的日志

---

## 2. 当前问题

### 2.1 Snapshot 只处理单实例状态

**现状**（supportlogindexandflush 分支）：

```rust
// raft/src/node.rs
let (collector, cf_tracker) = match (
    storage.get_logindex_collector(0),  // ❌ 只获取 instance_id=0
    storage.get_logindex_cf_tracker(0),
) {
    (Some(collector), Some(cf_tracker)) => (collector, cf_tracker),
    ...
};
```

```rust
// storage/src/checkpoint.rs
pub struct RaftSnapshotMeta {
    pub logindex_collector_state: Vec<String>,  // ❌ 只保存单个实例的状态
}
```

**影响**：
- Instance 1 和 Instance 2 的 logindex 状态丢失
- Follower 安装 snapshot 后，这两个实例的 collector/cf_tracker 从空开始
- 可能导致错误的 log purge（过早删除仍在其他实例中应用的 log）

### 2.2 数据完整性问题

假设场景：
```
Instance 0: applied_log_index = 100, flushed = 80
Instance 1: applied_log_index = 100, flushed = 95
Instance 2: applied_log_index = 100, flushed = 90
```

- 正确的 smallest_flushed = min(80, 95, 90) = 80
- 如果只追踪 Instance 0，smallest_flushed = 80 ✓ 正确

但如果：
```
Instance 0: flushed = 95
Instance 1: flushed = 80   ← 未被追踪
Instance 2: flushed = 90   ← 未被追踪
```

- 当前计算的 smallest = 95 ❌ 错误！
- 实际应该 = min(95, 80, 90) = 80
- Log 81-94 在 Instance 1 中未 flushed，不应被 purge

---

## 3. 完整方案

### 3.1 数据结构修改

#### RaftSnapshotMeta 支持多实例

```rust
// storage/src/checkpoint.rs
pub const CURRENT_SNAPSHOT_VERSION: u32 = 2;  // v1 → v2

/// Snapshot metadata with multi-instance logindex state
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RaftSnapshotMeta {
    /// Snapshot format version
    pub version: u32,
    /// Last log index included in the snapshot
    pub last_included_index: u64,
    /// Last log term included in the snapshot  
    pub last_included_term: u64,
    
    /// LogIndex collector state for each instance
    /// Format: Vec[instance_id] = Vec<"log_index:seqno">
    /// Length should match Storage.db_instance_num (typically 3)
    #[serde(default)]
    pub logindex_collector_states: Vec<Vec<String>>,
}
```

#### 新增辅助方法

```rust
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
    
    /// Restore single instance collector state
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
    
    /// Backward compatibility: read v1 format (single instance state)
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
}
```

### 3.2 Snapshot 创建流程

```rust
// raft/src/state_machine.rs
impl RaftSnapshotBuilder for KiwiSnapshotBuilder {
    async fn build_snapshot(&mut self) -> Result<Snapshot, StorageError> {
        let (last_idx, last_term) = self.last_applied
            .map(|l| (l.index, l.leader_id.term))
            .unwrap_or((0, 0));
        
        // Create meta with all instance states
        let raft_meta = RaftSnapshotMeta::with_all_instances(
            last_idx,
            last_term,
            &self.storage,
        );
        
        // Create checkpoint for all instances
        let temp_dir = tempfile::tempdir()?;
        self.storage.create_checkpoint(&temp_dir.path(), &raft_meta)?;
        
        // Pack to tar
        let bytes = pack_dir_to_vec(&temp_dir.path())?;
        
        // ...
    }
}
```

### 3.3 Snapshot 安装流程

```rust
// raft/src/state_machine.rs
impl RaftStateMachine for KiwiStateMachine {
    async fn install_snapshot(&mut self, meta: &SnapshotMeta, snapshot: SnapshotData) -> Result<()> {
        // Unpack snapshot
        let unpack_root = tempfile::tempdir()?;
        unpack_tar_to_dir(&snapshot, unpack_root.path())?;
        let checkpoint_root = unpacked_checkpoint_root(unpack_root.path())?;
        
        // Read meta
        let file_meta = RaftSnapshotMeta::read_from_dir(&checkpoint_root)?;
        
        // Restore checkpoint layout (all instances)
        restore_checkpoint_layout(
            &checkpoint_root,
            &self.db_path,
            self.storage.db_instance_num,
        )?;
        
        // Initialize cf_tracker from SST properties (all instances)
        self.storage.init_cf_trackers()?;
        
        // Restore collector state for all instances
        file_meta.restore_to_storage(&self.storage);
        
        self.last_applied = meta.last_log_id;
        // ...
    }
}
```

### 3.4 全局最小 flushed_log_index 计算

```rust
// storage/src/storage.rs
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

### 3.5 Snapshot Callback 触发机制

```rust
// storage/src/redis.rs - EventListener callback
let snapshot_callback: SnapshotCallback = Arc::new(move |log_index: i64, is_manual: bool| {
    // Check if snapshot should be created
    // Compare with snapshot_logs_threshold (e.g., 5000 logs since last snapshot)
    // This is called every 10 flushes by LogIndexAndSequenceCollectorPurger
    log::info!("Snapshot callback: log_index={}, is_manual={}", log_index, is_manual);
    
    // TODO: Connect to Raft's snapshot trigger
    // Option 1: Use a channel to notify Raft node
    // Option 2: Directly call raft.trigger_snapshot() if available
});

let flush_trigger: FlushTrigger = Arc::new(move |cf_id: usize| {
    // Trigger manual flush for specified CF
    log::info!("Flush trigger: cf_id={}", cf_id);
    
    // TODO: Implement actual flush
    // instance.db.as_ref().unwrap().flush_cf(cf_handle)?
});
```

---

## 4. 数据流（完整）

```
Write Request (slot_id)
    │
    ├─ slot_id % 3 → instance_id
    │
    ▼
Raft Log Replication
    │
    ▼
StateMachine.apply(raft_log_index)
    │
    ▼
Storage.on_binlog_write(binlog, raft_log_index)
    │
    ├─ instance[instance_id].batch.put(...)
    ├─ instance[instance_id].db.commit() → seqno
    ├─ instance[instance_id].collector.update(raft_log_index, seqno)
    │
    ▼
RocksDB Flush (per instance)
    │
    ▼
EventListener.on_flush_completed(cf_id, largest_seqno)
    │
    ├─ log_index = collector.find_applied_log_index(largest_seqno)
    ├─ cf_tracker[instance_id].set_flushed_log_index(cf_id, log_index, seqno)
    ├─ collector[instance_id].purge(smallest_applied_log_index)
    ├─ every 10 flushes: snapshot_callback(smallest_flushed_log_index)
    │
    ▼
Storage.get_global_smallest_flushed_log_index()
    │
    ├─ min over all instances: tracker[0..N].get_smallest_log_index()
    │
    ▼
Raft Log Purge (log_index < global_smallest_flushed)

Snapshot Creation (Leader):
    │
    ├─ RaftSnapshotMeta.with_all_instances(storage)
    │   ├─ collector[0].export_state()
    │   ├─ collector[1].export_state()
    │   ├─ collector[2].export_state()
    │
    ├─ Storage.create_checkpoint()
    │   ├─ instance[0].db.create_checkpoint(db_path/0)
    │   ├─ instance[1].db.create_checkpoint(db_path/1)
    │   ├─ instance[2].db.create_checkpoint(db_path/2)
    │
    ▼
Tar + Transfer to Follower

Snapshot Installation (Follower):
    │
    ├─ Untar to temp dir
    ├─ restore_checkpoint_layout()
    │   ├─ copy temp/0 → db_path/0
    │   ├─ copy temp/1 → db_path/1  
    │   ├─ copy temp/2 → db_path/2
    │
    ├─ Storage.init_cf_trackers()
    │   ├─ instance[0].cf_tracker.init(db[0])
    │   ├─ instance[1].cf_tracker.init(db[1])
    │   ├─ instance[2].cf_tracker.init(db[2])
    │
    ├─ RaftSnapshotMeta.restore_to_storage(storage)
    │   ├─ collector[0].restore(state[0])
    │   ├─ collector[1].restore(state[1])
    │   ├─ collector[2].restore(state[2])
    │
    ▼
Follower ready with correct logindex state
```

---

## 5. 修改清单

### 5.1 必要修改

| 文件 | 修改内容 | 优先级 |
|------|----------|--------|
| `storage/src/checkpoint.rs` | `RaftSnapshotMeta` 支持多实例 `Vec<Vec<String>>` | P0 |
| `storage/src/checkpoint.rs` | 新增 `with_all_instances()`, `restore_to_storage()` | P0 |
| `storage/src/storage.rs` | 新增 `get_global_smallest_flushed_log_index()` | P0 |
| `raft/src/state_machine.rs` | `build_snapshot` 使用新方法 | P0 |
| `raft/src/state_machine.rs` | `install_snapshot` 恢复所有实例 | P0 |
| `raft/src/node.rs` | 删除单实例 collector 获取，改用 Storage 方法 | P0 |

### 5.2 代码重复清理

| 操作 | 说明 |
|------|------|
| 删除 `raft/src/collector.rs` | 与 storage/logindex/collector.rs 重复 |
| 删除 `raft/src/cf_tracker.rs` | 与 storage/logindex/cf_tracker.rs 重复 |
| 删除 `raft/src/event_listener.rs` | 与 storage/logindex/event_listener.rs 重复 |
| 删除 `raft/src/table_properties.rs` | 与 storage/logindex/table_properties.rs 重复 |
| 修改 `raft/src/lib.rs` | 改为 `pub use storage::logindex::*` |
| 修改 `raft/tests/logindex_integration.rs` | 使用 storage::logindex 类型 |

### 5.3 功能完善

| 文件 | 修改内容 |
|------|----------|
| `storage/src/redis.rs` | 实现实际的 flush_trigger |
| `raft/src/node.rs` | 连接 snapshot_callback 到 Raft |

---

## 6. 测试计划

### 6.1 单元测试

```rust
#[test]
fn test_raft_snapshot_meta_multi_instance() {
    // Create 3 collectors with different states
    let c0 = Arc::new(LogIndexAndSequenceCollector::new(0));
    c0.update(100, 1000);
    
    let c1 = Arc::new(LogIndexAndSequenceCollector::new(0));
    c1.update(200, 2000);
    
    let c2 = Arc::new(LogIndexAndSequenceCollector::new(0));
    c2.update(300, 3000);
    
    // Create storage mock with 3 instances
    let meta = RaftSnapshotMeta::with_all_instances(300, 1, &storage);
    
    assert_eq!(meta.logindex_collector_states.len(), 3);
    assert!(meta.logindex_collector_states[0].contains(&"100:1000"));
    assert!(meta.logindex_collector_states[1].contains(&"200:2000"));
    assert!(meta.logindex_collector_states[2].contains(&"300:3000"));
}

#[test]
fn test_backward_compatibility_v1_format() {
    // Read old v1 snapshot (single instance state)
    let v1_state = vec!["100:1000".to_string(), "200:2000".to_string()];
    let meta = RaftSnapshotMeta::from_v1(200, 1, v1_state);
    
    // Should be converted to v2 format (instance 0 only)
    assert_eq!(meta.version, 2);
    assert_eq!(meta.logindex_collector_states.len(), 1);
}
```

### 6.2 集成测试

```rust
#[tokio::test]
async fn test_snapshot_with_multi_instance_logindex() {
    // Setup: 3 instances, data distributed by slot
    let storage = Storage::new(3, 0);
    storage.open(options, db_path)?;
    
    // Write data to different instances
    // slot 0 → instance 0
    // slot 1 → instance 1
    // slot 2 → instance 2
    storage.on_binlog_write(&binlog_slot0, 100)?;
    storage.on_binlog_write(&binlog_slot1, 200)?;
    storage.on_binlog_write(&binlog_slot2, 300)?;
    
    // Flush all instances
    for inst in &storage.insts {
        inst.db.unwrap().flush()?;
    }
    
    // Build snapshot
    let mut builder = sm.get_snapshot_builder().await;
    let snap = builder.build_snapshot().await?;
    
    // Verify: snapshot contains all 3 instance states
    let meta = parse_snapshot_meta(&snap);
    assert_eq!(meta.logindex_collector_states.len(), 3);
    
    // Install on follower
    let follower_storage = Storage::new(3, 0);
    sm2.install_snapshot(&snap.meta, snap.data).await?;
    
    // Verify: all instances restored correctly
    for i in 0..3 {
        let collector = follower_storage.get_logindex_collector(i).unwrap();
        // Verify state matches original
    }
}
```

### 6.3 性能测试

- Snapshot 创建时间：确保汇总 3 个实例状态不显著增加延迟
- Snapshot 安装时间：确保恢复 3 个实例状态不阻塞 Raft
- 内存占用：3 个 collector 状态序列化的内存开销

---

## 7. 验收标准

### 7.1 功能验收

| 标准 | 说明 |
|------|------|
| ✅ 多实例状态保存 | Snapshot 包含所有实例的 collector 状态 |
| ✅ 多实例状态恢复 | Follower 安装后所有实例状态正确 |
| ✅ 全局最小计算 | `get_global_smallest_flushed_log_index` 正确计算 |
| ✅ Log purge 正确 | 只 purge 所有实例都已 flushed 的 log |
| ✅ 向后兼容 | 能读取 v1 格式 snapshot |

### 7.2 测试验收

```bash
cargo test --package storage --lib checkpoint
cargo test --package raft --test snapshot_logindex_test
cargo test --package raft --test snapshot_roundtrip_test
cargo test --workspace
```

---

## 8. 参考资料

- 原方案文档：`docs/snapshot-logindex-sync-plan.md`
- 审查文档：`docs/superpowers/specs/2026-04-16-snapshot-logindex-sync-review.md`
- POC v2 设计：`kiwi-raft-poc-v2.md`
- 相关代码：
  - `storage/src/logindex/` - logindex 模块实现
  - `storage/src/checkpoint.rs` - snapshot 元数据
  - `raft/src/state_machine.rs` - snapshot 创建/安装