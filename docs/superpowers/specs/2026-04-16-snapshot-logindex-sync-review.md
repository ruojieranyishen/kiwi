# Snapshot LogIndex 同步方案 - 审查与改进

## 1. 方案审查总结

### 1.1 已实现的功能

当前代码已完成以下核心功能：

| 模块 | 文件 | 功能 | 状态 |
|------|------|------|------|
| Collector | `storage/src/logindex/collector.rs` | 维护 (log_index, seqno) 映射队列 | ✅ 完成 |
| CF Tracker | `storage/src/logindex/cf_tracker.rs` | 追踪每个 CF 的 applied/flushed log_index | ✅ 完成 |
| Event Listener | `storage/src/logindex/event_listener.rs` | Flush 完成时更新状态 | ✅ 完成 |
| Table Properties | `storage/src/logindex/table_properties.rs` | 写入 log_index 到 SST 属性 | ✅ 完成 |
| Redis 集成 | `storage/src/redis.rs` | 初始化 collector/cf_tracker/purger | ✅ 完成 |
| Storage 集成 | `storage/src/storage.rs` | on_binlog_write 更新 collector | ✅ 完成 |
| Snapshot Meta | `storage/src/checkpoint.rs` | RaftSnapshotMeta 包含 collector 状态 | ✅ 完成 |
| State Machine | `raft/src/state_machine.rs` | build/install snapshot 同步状态 | ✅ 完成 |

### 1.2 发现的问题

#### 🔴 P1 - Snapshot 只处理单实例状态

**背景**：
- main 分支已实现多实例分片（`on_binlog_write` 按 slot 分布到不同实例）
- POC v2 设计文档中"使用第一个实例"已过时
- 每个实例有独立的 RocksDB → 独立的 sequence number 序列 → 需要独立的 collector/cf_tracker
- **问题**：snapshot 创建时 `create_raft_node()` 只获取 instance_id=0 的状态
- **问题**：`RaftSnapshotMeta.logindex_collector_state` 只保存单个实例的状态

**影响**：
- 只有实例 0 的 logindex 状态被保存到 snapshot
- Follower 安装 snapshot 后，其他实例（1、2）的 collector/cf_tracker 未恢复
- 其他实例的 logindex 状态从空开始，导致错误的 log purge 决策

#### ⚠️ P2 - 代码重复

**问题描述**：
- `raft/src/` 下有独立的 collector.rs, cf_tracker.rs, event_listener.rs, table_properties.rs
- 这些文件与 `storage/src/logindex/` 下的实现重复
- 实际代码使用 `storage::logindex` 的类型

**影响**：
- 维护困难（两份代码需同步）
- 潜在的行为差异
- 编译产物膨胀

#### 📝 P3 - 文档不一致

文档第 4 部分（详细实现）与实际代码不一致：
- 第 4.1: 描述 storage 需要 `raft.workspace = true`（实际不需要）
- 第 4.4: 描述 node.rs 创建新 collector（实际从 storage 获取）
- 第 4.5: 描述 state_machine 初始化 collector（实际从构造函数接收）

---

## 2. 改进方案

### 2.1 多实例架构修复

**分析**：

每个 Redis 实例有独立的 RocksDB，因此：
- 每个实例有独立的 sequence number 序列（从 0 开始）
- 每个实例需要自己的 collector 来追踪 `(log_index, seqno)` 映射
- 共享 collector 会导致 seqno 混乱（不同实例可能有相同的 seqno 值）

**结论**：当前"每个实例有自己的 collector/cf_tracker"设计是正确的。

**真正的问题**：snapshot 创建/安装时只处理 instance_id=0，忽略了其他实例。

---

**方案：汇总所有实例状态到 snapshot**

Snapshot 创建时汇总所有实例的 collector 状态，安装时恢复到各实例。

```rust
// storage/src/checkpoint.rs
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RaftSnapshotMeta {
    pub version: u32,
    pub last_included_index: u64,
    pub last_included_term: u64,
    /// 每个实例的 collector 状态：Vec[instance_id] = Vec<"log_index:seqno">
    #[serde(default)]
    pub logindex_collector_states: Vec<Vec<String>>,
}

impl RaftSnapshotMeta {
    /// 创建 snapshot meta，包含所有实例的 collector 状态
    pub fn with_all_instance_collectors(
        last_included_index: u64,
        last_included_term: u64,
        storage: &Storage,
    ) -> Self {
        let states: Vec<Vec<String>> = (0..storage.db_instance_num)
            .map(|i| {
                storage.get_logindex_collector(i)
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

    /// 恢复所有实例的 collector 状态
    pub fn restore_all_collectors(&self, storage: &Storage) {
        for (i, state) in self.logindex_collector_states.iter().enumerate() {
            if let Some(collector) = storage.get_logindex_collector(i) {
                for entry in state {
                    if let Some((log_index_str, seqno_str)) = entry.split_once(':') {
                        if let (Ok(log_index), Ok(seqno)) = (
                            log_index_str.parse::<i64>(),
                            seqno_str.parse::<u64>()
                        ) {
                            collector.update(log_index, seqno);
                        }
                    }
                }
            }
        }
    }
}
```

```rust
// raft/src/state_machine.rs
impl RaftSnapshotBuilder for KiwiSnapshotBuilder {
    async fn build_snapshot(&mut self) -> Result<Snapshot, StorageError> {
        // ...
        
        // 汇总所有实例的 collector 状态
        let raft_meta = RaftSnapshotMeta::with_all_instance_collectors(
            last_idx,
            last_term,
            &self._storage,
        );
        
        self._storage.create_checkpoint(&dir, &raft_meta)?;
        // ...
    }
}

impl KiwiStateMachine {
    async fn install_snapshot(&mut self, ...) -> Result<()> {
        // ...
        
        // 初始化所有实例的 cf_tracker（从 SST 属性）
        self.storage.init_cf_trackers()?;
        
        // 恢复所有实例的 collector 状态
        file_meta.restore_all_collectors(&self.storage);
        
        // ...
    }
}
```

**优点**：
- 保持每个实例独立的 collector/cf_tracker（正确的设计）
- Snapshot 包含完整的多实例状态
- Follower 安装后所有实例状态正确恢复

**注意点**：
- 恢复后需要确保各实例的 collector 与 cf_tracker 状态一致
- 如果 snapshot 中实例数量与当前节点不同，需要处理（通常应该相同）

---

**方案 B（备选）**：在 Storage 层统一追踪 smallest_applied_log_index

如果只需要追踪全局最小 log_index 用于 purge，可以在 Storage 层汇总：

```rust
// storage/src/storage.rs
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
```

当前代码已实现此方法（第 498-513 行），但未被 snapshot 相关逻辑正确使用。

---

### 2.2 代码重复清理

删除 `raft/src/` 下的重复文件，改为重新导出 storage 模块的类型。

```rust
// raft/src/lib.rs
// 删除这些模块定义
// pub mod collector;
// pub mod cf_tracker;
// pub mod event_listener;
// pub mod table_properties;

// 直接重新导出 storage 的实现
pub use storage::logindex::{
    LogIndexAndSequenceCollector,
    LogIndexOfColumnFamilies,
    LogIndexAndSequenceCollectorPurger,
    LogIndexTablePropertiesCollectorFactory,
    LogIndexSeqnoPair,
    LogIndexAndSequencePair,
    LogIndex,
    SequenceNumber,
    PROPERTY_KEY,
    get_largest_log_index_from_collection,
    read_stats_from_table_props,
};

// 保留 CF_NAMES 常量用于内部使用
pub const COLUMN_FAMILY_COUNT: usize = storage::ColumnFamilyIndex::COUNT;
pub const CF_NAMES: [&str; COLUMN_FAMILY_COUNT] = [
    "default", "hash_data_cf", "set_data_cf", 
    "list_data_cf", "zset_data_cf", "zset_score_cf",
];
```

**需要删除的文件**：
- `raft/src/collector.rs`
- `raft/src/cf_tracker.rs`
- `raft/src/event_listener.rs`
- `raft/src/table_properties.rs`
- `raft/src/types.rs`（如与 storage/logindex/types.rs 重复）
- `raft/src/db_access.rs`（如与 storage/logindex/db_access.rs 重复）

**需要更新的文件**：
- `raft/src/lib.rs` - 更新导出
- `raft/tests/logindex_integration.rs` - 改为使用 storage::logindex

---

### 2.3 Snapshot Callback 和 Flush Trigger 实现

当前 `redis.rs` 中的 callback 只是 log 输出，需要实现实际功能。

```rust
// storage/src/redis.rs::open()
// 当前实现（仅 log）
let snapshot_callback: SnapshotCallback = Arc::new(move |log_index: i64, _is_manual: bool| {
    log::info!("LogIndex snapshot callback triggered: log_index={}", log_index);
    // TODO(#LOGINDEX-1): 实际触发 snapshot
});

// 改进方案：传递实际的触发函数
// 由 node.rs 创建时传入
pub struct LogIndexCallbacks {
    trigger_snapshot: Option<Arc<dyn Fn(i64) + Send + Sync>>,
    trigger_flush: Option<Arc<dyn Fn(usize) + Send + Sync>>,
}

impl Redis {
    pub fn set_logindex_callbacks(&mut self, callbacks: LogIndexCallbacks) {
        // 更新 purger 中的 callback
    }
}
```

---

## 3. 正确的架构描述

### 3.1 数据流

```
Write Request → Raft Log → State Machine.apply()
                               ↓
                    Storage.on_binlog_write(raft_log_index)
                               ↓
              ┌────────────────┴────────────────┐
              │  Batch write to instance[slot%N] │
              │  Update shared_collector         │
              │    (log_index, seqno)            │
              └────────────────┴────────────────┘
                               ↓
                        RocksDB Flush
                               ↓
              EventListener.on_flush_completed()
              ┌────────────────┴────────────────┐
              │ cf_tracker.update()             │
              │ collector.purge()               │
              │ trigger_snapshot (every 10)     │
              └────────────────┴────────────────┘
                               ↓
                    get_smallest_applied_log_index()
                               ↓
                        Purge Raft Logs

Snapshot Creation:
    KiwiSnapshotBuilder.build_snapshot()
        → RaftSnapshotMeta.with_collector_state(shared_collector)
        → Storage.create_checkpoint() (所有实例)
        → pack to tar

Snapshot Restore:
    KiwiStateMachine.install_snapshot()
        → unpack tar
        → restore_checkpoint_layout() (所有实例)
        → init_cf_trackers() (从 SST 属性)
        → restore_collector_state() (从 meta)
```

### 3.2 组件职责

| 组件 | 位置 | 职责 |
|------|------|------|
| `Storage` | storage/src/storage.rs | 统一管理共享的 collector/cf_tracker |
| `Redis` | storage/src/redis.rs | RocksDB 实例，集成 event_listener |
| `KiwiStateMachine` | raft/src/state_machine.rs | Raft snapshot 创建/安装，同步 logindex |
| `RaftSnapshotMeta` | storage/src/checkpoint.rs | Snapshot 元数据，包含 collector 状态 |
| `LogIndexAndSequenceCollector` | storage/src/logindex/ | 维护 (log_index, seqno) 映射 |
| `LogIndexOfColumnFamilies` | storage/src/logindex/ | 追踪 CF 的 applied/flushed 状态 |
| `LogIndexAndSequenceCollectorPurger` | storage/src/logindex/ | Flush 完成时的处理逻辑 |

### 3.3 关键依赖关系

```
raft (依赖 storage)
    └── KiwiStateMachine
        └── 使用 storage::logindex 类型
        └── 持有 storage::Storage 引用
        
storage
    ├── logindex 模块 (独立实现)
    ├── Redis 实例数组
    └── 共享的 collector/cf_tracker

正确关系：
    raft 依赖 storage → 使用 storage::logindex
    不需要 storage 依赖 raft
```

---

## 4. 修改清单

### 4.1 必要修改（解决 P1 问题）

| 文件 | 修改内容 |
|------|----------|
| `storage/src/checkpoint.rs` | `RaftSnapshotMeta.logindex_collector_state` 改为 `Vec<Vec<String>>` 支持多实例 |
| `storage/src/checkpoint.rs` | 新增 `with_all_instance_collectors()` 方法 |
| `storage/src/checkpoint.rs` | 新增 `restore_all_collectors()` 方法 |
| `raft/src/state_machine.rs` | `build_snapshot()` 使用新方法汇总所有实例 |
| `raft/src/state_machine.rs` | `install_snapshot()` 使用新方法恢复所有实例 |

**说明**：保持每个实例独立的 collector/cf_tracker 设计不变（这是正确的），只需修改 snapshot 创建/安装逻辑来处理所有实例。

### 4.2 建议修改（解决 P2 问题）

| 文件 | 修改内容 |
|------|----------|
| 删除 `raft/src/collector.rs` | 重复实现 |
| 删除 `raft/src/cf_tracker.rs` | 重复实现 |
| 删除 `raft/src/event_listener.rs` | 重复实现 |
| 删除 `raft/src/table_properties.rs` | 重复实现 |
| 修改 `raft/src/lib.rs` | 改为重新导出 storage::logindex |
| 修改 `raft/tests/logindex_integration.rs` | 使用 storage::logindex |

### 4.3 功能完善（解决 TODO）

| 文件 | 修改内容 |
|------|----------|
| `storage/src/redis.rs` | 实现实际的 snapshot callback 和 flush trigger |
| `storage/src/logindex/event_listener.rs` | 连接到 Raft 的 snapshot 触发机制 |

---

## 5. 验收标准

### 5.1 功能验收

- [ ] 所有 Redis 实例的 logindex 状态被统一追踪
- [ ] Snapshot 包含所有实例的 logindex 状态
- [ ] Follower 安装 snapshot 后正确恢复 logindex 状态
- [ ] Log purge 基于 correct smallest_applied_log_index

### 5.2 测试验收

- [ ] 多实例场景下的 snapshot roundtrip 测试
- [ ] 数据分布在多个实例时的 log purge 测试
- [ ] `cargo test --workspace` 全部通过

### 5.3 代码质量

- [ ] 删除所有重复代码
- [ ] 文档与代码一致
- [ ] 无编译警告

---

## 6. 分支状态

| 分支 | 多实例分片 | logindex 集成 | 说明 |
|------|-----------|--------------|------|
| `main` | ✅ 已实现 | ❌ 未集成 | 基础功能 |
| `supportlogindexandflush` | ✅ 已实现 | ✅ 已集成 | 当前开发分支 |

**注意**：POC v2 设计文档中描述的"使用第一个实例"已过时，main 分支已进入多实例分片阶段。

---

## 7. 参考资料

- 原方案文档：`docs/snapshot-logindex-sync-plan.md`
- openraft 源码：`/home/lpf/dev/openraft`
- 相关测试：
  - `src/raft/tests/snapshot_logindex_test.rs`
  - `src/raft/tests/snapshot_roundtrip_test.rs`
  - `src/storage/tests/checkpoint_test.rs`