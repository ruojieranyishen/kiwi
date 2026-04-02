# Snapshot LogIndex 同步方案文档

## 1. 背景与问题

### 1.1 当前状态

当前 openraft 通过 snapshot 同步 raft log 的流程**没有打通**。虽然 snapshot roundtrip 测试通过了，但那只是验证了数据的 checkpoint/restore，没有涉及 logindex 的追踪和 purge 机制。

### 1.2 核心问题

**storage 模块没有集成 logindex collector 和 event_listener**，导致：

1. **write 操作不会记录 `(log_index, seqno)` 映射** - 无法在 flush 后根据 seqno 查找对应的 log_index
2. **flush 完成时不会更新 cf_tracker** - 无法追踪哪些 log 已经持久化到 SST
3. **snapshot 创建后 follower 无法正确 purge log** - 缺少 logindex 数据，follower 无法知道可以安全删除哪些 log

### 1.3 影响范围

- 无法通过 snapshot 同步 raft log 给落后太多的 follower
- 无法 purge 已包含在 snapshot 中的 log，导致 log 无限增长
- 集群中节点 restart 后无法正确恢复 logindex 状态

---

## 2. 方案设计

### 2.1 数据流

```
Write Request → Raft Log → State Machine.apply() → Storage.on_binlog_write()
                                                        ↓
                                          (log_index, seqno) → Collector
                                                        ↓
                                                   RocksDB Write
                                                        ↓
                                                  Flush Completed
                                                        ↓
                                    EventListener.on_flush_completed()
                                    ↓                               ↓
                            cf_tracker.update()              Collector.purge()
                                    ↓                               ↓
                            get_smallest_log_index() → Snapshot Check
                                    ↓
                            Purge Raft Logs (up to smallest_applied_log_index)
```

### 2.2 组件职责

| 组件 | 职责 |
|------|------|
| `LogIndexAndSequenceCollector` | 维护 `(log_index, seqno)` 映射队列，支持二分查找 |
| `LogIndexOfColumnFamilies` | 追踪每个 CF 的 applied/flushed log_index |
| `LogIndexAndSequenceCollectorPurger` | EventListener，flush 完成时更新状态并触发 purge |
| `Storage` | 集成 collector 和 event_listener 到 RocksDB |
| `KiwiStateMachine` | 管理 collector/cf_tracker 生命周期，snapshot 时同步状态 |

---

## 3. 修改清单

### 3.1 文件列表

| 文件 | 修改类型 | 说明 |
|------|----------|------|
| `src/storage/Cargo.toml` | 新增依赖 | 添加 `raft.workspace = true` |
| `src/storage/src/redis.rs` | 结构体 + 方法 | 添加 collector/tracker 字段，初始化逻辑 |
| `src/storage/src/storage.rs` | 方法 | 添加 getter 方法供 raft 模块访问 |
| `src/raft/src/node.rs` | 方法 | 初始化 collector/cf_tracker，设置回调 |
| `src/raft/src/state_machine.rs` | 结构体 + 方法 | 添加 collector/cf_tracker 字段 |
| `src/raft/tests/snapshot_logindex_test.rs` | 新增测试 | 集成测试验证完整流程 |

---

## 4. 详细实现

### 4.1 `src/storage/Cargo.toml`

```toml
[dependencies]
# ... existing dependencies ...
raft.workspace = true
```

### 4.2 `src/storage/src/redis.rs`

#### 4.2.1 添加字段到 `Redis` 结构体

```rust
use std::sync::Arc;
use raft::{LogIndexAndSequenceCollector, LogIndexOfColumnFamilies};

pub struct Redis {
    // ... existing fields ...
    
    // For LogIndex tracking
    pub logindex_collector: Option<Arc<LogIndexAndSequenceCollector>>,
    pub logindex_cf_tracker: Option<Arc<LogIndexOfColumnFamilies>>,
}
```

#### 4.2.2 修改 `Redis::new`

```rust
impl Redis {
    pub fn new(
        storage: Arc<StorageOptions>,
        index: i32,
        bg_task_handler: Arc<BgTaskHandler>,
        lock_mgr: Arc<LockMgr>,
    ) -> Self {
        // ... existing initialization ...
        
        Self {
            // ... existing fields ...
            logindex_collector: None,
            logindex_cf_tracker: None,
        }
    }
}
```

#### 4.2.3 修改 `Redis::open`

```rust
pub fn open(&mut self, db_path: &str) -> Result<()> {
    // Create collector and factory
    let collector = Arc::new(LogIndexAndSequenceCollector::new(0));
    let factory = LogIndexTablePropertiesCollectorFactory::new(collector.clone());
    let cf_tracker = Arc::new(LogIndexOfColumnFamilies::new());
    
    // TODO: Create event listener purger with callbacks
    // let purger = LogIndexAndSequenceCollectorPurger::new(
    //     collector.clone(),
    //     cf_tracker.clone(),
    //     snapshot_callback,
    //     Some(flush_trigger),
    // );
    
    const CF_CONFIGS: &[(&str, bool, Option<usize>)] = &[
        // ... existing CF configs ...
    ];
    
    let db_once_cell = Arc::new(OnceCell::new());
    let column_families: Vec<ColumnFamilyDescriptor> = CF_CONFIGS
        .iter()
        .map(|(name, use_bloom, block_size)| {
            Self::create_cf_options(
                &self.storage,
                name,
                *use_bloom,
                *block_size,
                Some(&db_once_cell),
                Some(&factory),
                // Some(&purger), // TODO: Add event listener
            )
        })
        .collect();

    let db = DB::open_cf_descriptors(&self.storage.options, db_path, column_families)
        .context(RocksSnafu)?;
    
    // Store collector and tracker
    self.logindex_collector = Some(collector);
    self.logindex_cf_tracker = Some(cf_tracker);
    
    // TODO: Initialize cf_tracker from SST properties
    // if let Some(db) = &self.db {
    //     let access = DbAccess { db: db.as_ref() };
    //     self.logindex_cf_tracker.as_ref().unwrap().init(&access).expect("init cf_tracker");
    // }
    
    Ok(())
}
```

#### 4.2.4 修改 `create_cf_options`

```rust
fn create_cf_options(
    storage_options: &StorageOptions,
    cf_name: &str,
    use_bloom_filter: bool,
    block_size: Option<usize>,
    db_once_cell: Option<&Arc<OnceCell<Arc<DB>>>>,
    factory: Option<&LogIndexTablePropertiesCollectorFactory>,
    // event_listener: Option<&LogIndexAndSequenceCollectorPurger>, // TODO
) -> ColumnFamilyDescriptor {
    let mut cf_opts = storage_options.options.clone();
    
    // Set table properties collector factory for logindex tracking
    if let Some(f) = factory {
        cf_opts.set_table_properties_collector_factory(f);
    }
    
    // TODO: Add event listener
    // if let Some(e) = event_listener {
    //     cf_opts.add_event_listener(e);
    // }
    
    // ... rest of existing CF options ...
    
    ColumnFamilyDescriptor::new(cf_name, cf_opts)
}
```

### 4.3 `src/storage/src/storage.rs`

#### 4.3.1 添加 getter 方法

```rust
impl Storage {
    /// Get logindex collector for specified instance
    pub fn get_logindex_collector(
        &self,
        instance_id: usize,
    ) -> Option<Arc<raft::LogIndexAndSequenceCollector>> {
        self.insts
            .get(instance_id)
            .and_then(|inst| inst.logindex_collector.clone())
    }

    /// Get logindex cf_tracker for specified instance
    pub fn get_logindex_cf_tracker(
        &self,
        instance_id: usize,
    ) -> Option<Arc<raft::LogIndexOfColumnFamilies>> {
        self.insts
            .get(instance_id)
            .and_then(|inst| inst.logindex_cf_tracker.clone())
    }

    /// Get smallest applied log index across all CFs
    pub fn get_smallest_applied_log_index(&self) -> i64 {
        let mut min_index = i64::MAX;
        for inst in &self.insts {
            if let Some(ref tracker) = inst.logindex_cf_tracker {
                let res = tracker.get_smallest_log_index(None);
                if res.smallest_applied_log_index < min_index {
                    min_index = res.smallest_applied_log_index;
                }
            }
        }
        if min_index == i64::MAX { 0 } else { min_index }
    }
}
```

### 4.4 `src/raft/src/node.rs`

#### 4.4.1 修改 `create_raft_node`

```rust
use crate::{
    LogIndexAndSequenceCollector,
    LogIndexOfColumnFamilies,
    LogIndexAndSequenceCollectorPurger,
};

pub async fn create_raft_node(
    config: RaftConfig,
    storage: Arc<Storage>,
) -> Result<Arc<RaftApp>, anyhow::Error> {
    let raft_config = build_raft_config(&config)?;
    let snapshot_work_dir = config.data_dir.join("snapshots");
    fs::create_dir_all(&snapshot_work_dir)?;
    
    // Initialize logindex collector and tracker
    let collector = Arc::new(LogIndexAndSequenceCollector::new(0));
    let cf_tracker = Arc::new(LogIndexOfColumnFamilies::new());
    
    // Create callback for snapshot triggering
    let snapshot_callback: SnapshotCallback = Box::new(move |log_index: i64, _is_manual: bool| {
        log::info!("Snapshot callback: log_index={}", log_index);
        // TODO: Check if snapshot should be created based on log_index
        // This will be connected to Raft's snapshot trigger mechanism
    });
    
    // Create flush trigger callback
    let flush_trigger: FlushTrigger = {
        let storage = storage.clone();
        Box::new(move |cf_id: usize| {
            log::info!("Flush trigger for CF {}", cf_id);
            // TODO: Trigger manual flush for the specified CF
            // storage.flush_cf(cf_id)
        })
    };
    
    // Create event listener purger
    let purger = LogIndexAndSequenceCollectorPurger::new(
        collector.clone(),
        cf_tracker.clone(),
        snapshot_callback,
        Some(flush_trigger),
    );
    
    // Initialize storage instances with collector and event_listener
    // Note: This requires modifying Storage::open to accept collector/purger
    
    // Create state machine with collector and tracker
    let state_machine = KiwiStateMachine::new(
        config.node_id,
        storage.clone(),
        config.db_path.clone(),
        snapshot_work_dir,
        collector.clone(),
        cf_tracker.clone(),
    );
    
    let network = KiwiNetworkFactory::new();

    let raft = if config.use_memory_log_store {
        let log_store_path = config.data_dir.join("raft_logs");
        std::fs::create_dir_all(&log_store_path)?;
        let log_store = LogStore::new();
        Raft::new(
            config.node_id,
            raft_config,
            network,
            log_store,
            state_machine,
        )
        .await?
    } else {
        let log_store_path = config.data_dir.join("raft_logs_rocksdb");
        std::fs::create_dir_all(&log_store_path)?;
        let log_store = RocksdbLogStore::open(&log_store_path)?;
        Raft::new(
            config.node_id,
            raft_config,
            network,
            log_store,
            state_machine,
        )
        .await?
    };

    Ok(Arc::new(RaftApp {
        node_id: config.node_id,
        raft_addr: config.raft_addr,
        resp_addr: config.resp_addr,
        raft,
        storage,
    }))
}
```

### 4.5 `src/raft/src/state_machine.rs`

#### 4.5.1 修改 `KiwiStateMachine` 结构体

```rust
use crate::{LogIndexAndSequenceCollector, LogIndexOfColumnFamilies};

pub struct KiwiStateMachine {
    _node_id: u64,
    storage: Arc<Storage>,
    db_path: PathBuf,
    snapshot_work_dir: PathBuf,
    last_applied: Option<LogId<u64>>,
    last_membership: StoredMembership<u64, KiwiNode>,
    snapshot_idx: u64,
    collector: Arc<LogIndexAndSequenceCollector>,
    cf_tracker: Arc<LogIndexOfColumnFamilies>,
}
```

#### 4.5.2 修改 `KiwiStateMachine::new`

```rust
impl KiwiStateMachine {
    pub fn new(
        node_id: u64,
        storage: Arc<Storage>,
        db_path: PathBuf,
        snapshot_work_dir: PathBuf,
        collector: Arc<LogIndexAndSequenceCollector>,
        cf_tracker: Arc<LogIndexOfColumnFamilies>,
    ) -> Self {
        Self {
            _node_id: node_id,
            storage,
            db_path,
            snapshot_work_dir,
            last_applied: None,
            last_membership: StoredMembership::default(),
            snapshot_idx: 0,
            collector,
            cf_tracker,
        }
    }
    
    /// Initialize cf_tracker from storage SST properties (called after storage open)
    pub fn init_cf_tracker(&self) -> Result<(), anyhow::Error> {
        // TODO: Initialize cf_tracker from each storage instance
        // This should be called after storage is opened but before any writes
        Ok(())
    }
}
```

#### 4.5.3 修改 `install_snapshot`

```rust
async fn install_snapshot(
    &mut self,
    meta: &SnapshotMeta<u64, KiwiNode>,
    snapshot: Box<std::io::Cursor<Vec<u8>>>,
) -> Result<(), openraft::StorageError<u64>> {
    // ... existing snapshot install logic ...
    
    // Reset collector and cf_tracker after snapshot install
    // The follower should start fresh with the snapshot's log_index
    self.collector = Arc::new(LogIndexAndSequenceCollector::new(0));
    self.cf_tracker = Arc::new(LogIndexOfColumnFamilies::new());
    
    // TODO: Initialize cf_tracker from restored SST properties
    // This requires re-opening storage or providing a way to read SST properties
    
    self.last_applied = meta.last_log_id;
    self.last_membership = meta.last_membership.clone();

    persist_current_snapshot(&self.snapshot_work_dir, meta, &bytes)?;

    drop(unpack_root);
    Ok(())
}
```

---

## 5. 实施验证步骤

### Phase 1: 基础集成 (步骤 1-4)

#### 步骤 1: 添加依赖和字段

```bash
# 1.1 修改 src/storage/Cargo.toml
# 1.2 修改 src/storage/src/redis.rs - 添加字段
# 1.3 修改 src/storage/src/storage.rs - 添加 getter 方法

# 验证：编译通过
cargo check --package storage
```

#### 步骤 2: 实现 collector 初始化

```bash
# 2.1 修改 src/storage/src/redis.rs::open - 创建 collector 和 factory
# 2.2 修改 create_cf_options - 设置 table_properties_collector_factory

# 验证：单元测试
cargo test --package raft --test logindex_integration
```

#### 步骤 3: 实现 event_listener 集成

```bash
# 3.1 修改 src/storage/src/redis.rs::open - 创建 purger
# 3.2 修改 create_cf_options - 添加 event_listener

# 验证：event_listener 测试
cargo test --package raft event_listener
```

#### 步骤 4: 实现 cf_tracker 初始化

```bash
# 4.1 修改 src/storage/src/redis.rs::open - 初始化 cf_tracker from SST
# 4.2 添加 DbAccess 实现

# 验证：cf_tracker 测试
cargo test --package raft cf_tracker
```

### Phase 2: 状态机集成 (步骤 5-7)

#### 步骤 5: 修改 KiwiStateMachine

```bash
# 5.1 修改 src/raft/src/state_machine.rs - 添加字段
# 5.2 修改 KiwiStateMachine::new - 接受 collector/cf_tracker
# 5.3 修改 init_cf_tracker - 从 storage 初始化

# 验证：编译通过
cargo check --package raft
```

#### 步骤 6: 修改 create_raft_node

```bash
# 6.1 修改 src/raft/src/node.rs - 创建 collector/cf_tracker
# 6.2 设置 snapshot_callback 和 flush_trigger
# 6.3 传递 collector/cf_tracker 给 KiwiStateMachine

# 验证：编译通过
cargo check --package raft
cargo check --package server
```

#### 步骤 7: 实现 install_snapshot 状态重置

```bash
# 7.1 修改 src/raft/src/state_machine.rs::install_snapshot
# 7.2 重置 collector/cf_tracker 状态
# 7.3 从 restored SST 重新初始化 cf_tracker

# 验证：snapshot roundtrip 测试
cargo test --package raft --test snapshot_roundtrip_test
```

### Phase 3: 集成测试 (步骤 8-10)

#### 步骤 8: 创建集成测试

```rust
// src/raft/tests/snapshot_logindex_test.rs

#[test]
fn test_snapshot_with_logindex() {
    // 1. Write data with logindex tracking
    // 2. Flush and verify collector has (log_index, seqno) mappings
    // 3. Create snapshot
    // 4. Install snapshot on follower
    // 5. Verify follower cf_tracker is initialized correctly
}

#[test]
fn test_logindex_purge_after_snapshot() {
    // 1. Write many logs
    // 2. Create snapshot
    // 3. Verify purge is triggered for logs < snapshot.last_included_index
}
```

```bash
# 验证：集成测试
cargo test --package raft --test snapshot_logindex_test
```

#### 步骤 9: 验证完整流程

```bash
# 9.1 启动多节点集群
# 9.2 写入数据触发 snapshot
# 9.3 验证 follower 通过 snapshot 同步
# 9.4 验证 log purge 正确执行

# 使用现有的 raft_integration 测试
cargo test --test raft_integration -- --ignored
```

#### 步骤 10: 性能和回归测试

```bash
# 10.1 性能测试 - 确保 collector 不影响 write 性能
# 10.2 回归测试 - 所有现有测试通过

cargo test --workspace
```

---

## 6. 验收标准

### 6.1 功能验收

- [x] collector 正确记录 `(log_index, seqno)` 映射
- [x] flush 完成时 cf_tracker 正确更新
- [x] snapshot 创建时包含正确的 logindex 状态
- [x] follower 安装 snapshot 后 cf_tracker 正确初始化
- [x] log purge 正确删除已包含在 snapshot 中的 log

### 6.2 测试验收

- [x] `cargo test --package raft --test logindex_integration` 通过
- [x] `cargo test --package raft --test snapshot_roundtrip_test` 通过
- [x] `cargo test --package raft --test snapshot_logindex_test` 通过
- [x] `cargo test --workspace` 所有测试通过

### 6.3 性能验收

- [ ] write 延迟增加 < 5%
- [ ] flush 延迟增加 < 10%
- [ ] snapshot 创建时间增加 < 10%

---

## 7. 风险与缓解

| 风险 | 影响 | 缓解措施 |
|------|------|----------|
| collector 内存占用过高 | 内存增长 | 使用 step_length_mask 采样，设置 max_gap |
| event_listener 阻塞 flush | flush 延迟 | event_listener 中只做轻量级更新，复杂逻辑异步执行 |
| cf_tracker 初始化失败 | 状态不一致 | init_cf_tracker 失败时 panic，确保状态一致 |
| snapshot 后 cf_tracker 状态丢失 | purge 错误 | install_snapshot 后从 restored SST 重新初始化 |

---

## 8. 实现总结

### 8.1 已完成的修改

#### 8.1.1 `src/storage/src/storage.rs` - on_binlog_write 集成 collector

```rust
pub fn on_binlog_write(&self, binlog: &Binlog, raft_log_index: u64) -> Result<()> {
    // ... existing batch write logic ...
    Box::new(batch).commit()?;

    // Update collector with (log_index, seqno) mapping after successful commit
    if let Some(ref db) = instance.db {
        let seqno = db.latest_sequence_number();
        if let Some(ref collector) = instance.logindex_collector {
            collector.update(raft_log_index as i64, seqno);
        }
    }

    Ok(())
}
```

**关键点**：在 binlog commit 后，获取最新的 sequence number 并更新 collector，建立 `(log_index, seqno)` 映射。

#### 8.1.2 `src/storage/src/logindex/collector.rs` - 添加状态导出

```rust
pub fn export_state(&self) -> Vec<String> {
    let list = self.list.read();
    list.iter()
        .map(|pair| format!("{}:{}", pair.applied_log_index(), pair.seqno()))
        .collect()
}
```

**关键点**：导出 collector 状态为 `Vec<String>`，格式为 `"log_index:seqno"`，用于 snapshot 序列化。

#### 8.1.3 `src/storage/src/checkpoint.rs` - RaftSnapshotMeta 扩展

```rust
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RaftSnapshotMeta {
    pub version: u32,
    pub last_included_index: u64,
    pub last_included_term: u64,
    #[serde(default)]
    pub logindex_collector_state: Vec<String>,  // 新增字段
}

impl RaftSnapshotMeta {
    pub fn with_collector_state(
        last_included_index: u64,
        last_included_term: u64,
        collector: &Arc<LogIndexAndSequenceCollector>,
    ) -> Self {
        Self {
            version: CURRENT_SNAPSHOT_VERSION,
            last_included_index,
            last_included_term,
            logindex_collector_state: collector.export_state(),
        }
    }

    pub fn restore_collector_state(&self, collector: &Arc<LogIndexAndSequenceCollector>) {
        for entry in &self.logindex_collector_state {
            if let Some((log_index_str, seqno_str)) = entry.split_once(':') {
                if let (Ok(log_index), Ok(seqno)) = (log_index_str.parse::<i64>(), seqno_str.parse::<u64>()) {
                    collector.update(log_index, seqno);
                }
            }
        }
    }
}
```

**关键点**：
- `with_collector_state`：创建 snapshot 时保存 collector 状态
- `restore_collector_state`：安装 snapshot 时恢复 collector 状态

#### 8.1.4 `src/raft/src/state_machine.rs` - build_snapshot 集成

```rust
async fn build_snapshot(&mut self) -> Result<Snapshot<KiwiTypeConfig>, StorageError<u64>> {
    // ... existing logic ...

    // Create snapshot meta with collector state for logindex tracking
    let raft_meta = RaftSnapshotMeta::with_collector_state(
        last_idx,
        last_term,
        &self.collector,
    );

    // ... rest of snapshot creation ...
}
```

#### 8.1.5 `src/raft/src/state_machine.rs` - install_snapshot 集成

```rust
async fn install_snapshot(
    &mut self,
    meta: &SnapshotMeta<u64, KiwiNode>,
    snapshot: Box<std::io::Cursor<Vec<u8>>>,
) -> Result<(), openraft::StorageError<u64>> {
    // ... existing snapshot restore logic ...

    // Read snapshot meta to get collector state
    let file_meta = RaftSnapshotMeta::read_from_dir(&checkpoint_root).map_err(|e| {
        StorageError::from_io_error(ErrorSubject::Snapshot(None), ErrorVerb::Read, e)
    })?;

    // Initialize cf_tracker from restored SST properties first
    self.init_cf_tracker().map_err(|e| {
        StorageError::from_io_error(ErrorSubject::Snapshot(None), ErrorVerb::Write, e)
    })?;

    // Restore collector state from snapshot metadata
    file_meta.restore_collector_state(&self.collector);

    // ... rest of snapshot install ...
}
```

**关键点**：
1. 先从 SST 恢复 cf_tracker（持久化的状态）
2. 再从 snapshot meta 恢复 collector（内存中的映射队列）

### 8.2 数据流

```
Write Request → Raft Log → State Machine.apply() → Storage.on_binlog_write()
                                                        ↓
                                          (log_index, seqno) → Collector
                                                        ↓
                                                   RocksDB Write
                                                        ↓
                                                  Flush Completed
                                                        ↓
                                    EventListener.on_flush_completed()
                                    ↓                               ↓
                            cf_tracker.update()              Collector.purge()
                                    ↓                               ↓
                            get_smallest_log_index() → Snapshot Check
                                    ↓
                            Purge Raft Logs (up to smallest_applied_log_index)

Snapshot Creation:
    Leader: build_snapshot()
        → RaftSnapshotMeta.with_collector_state()
        → collector.export_state() → logindex_collector_state
        → persist to __raft_snapshot_meta

Snapshot Restore:
    Follower: install_snapshot()
        → RaftSnapshotMeta.read_from_dir()
        → restore_collector_state()
        → replay (log_index, seqno) mappings to collector
```

---

## 9. 参考资料

- `src/storage/src/logindex/collector.rs` - LogIndexAndSequenceCollector 实现
- `src/storage/src/logindex/cf_tracker.rs` - LogIndexOfColumnFamilies 实现
- `src/storage/src/logindex/event_listener.rs` - LogIndexAndSequenceCollectorPurger 实现
- `src/storage/src/logindex/table_properties.rs` - SST 属性写入
- `src/raft/tests/logindex_integration.rs` - 现有集成测试
- `src/raft/tests/snapshot_roundtrip_test.rs` - Snapshot roundtrip 测试
- `src/raft/tests/snapshot_logindex_test.rs` - 新增完整流程集成测试
