# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Commands

- Fastest development check: `cargo check`
- Preferred local workflow helpers:
  - `./scripts/dev.sh check`
  - `./scripts/dev.sh build`
  - `./scripts/dev.sh run`
  - `./scripts/dev.sh watch`
  - `./scripts/dev.sh test`
- Makefile shortcuts:
  - `make build`
  - `make release`
  - `make run`
  - `make test`
  - `make fmt`
  - `make fmt-check`
  - `make lint`
- Run the server directly:
  - `cargo run --bin kiwi`
  - `cargo run --release --bin kiwi`
- Run with config:
  - `cargo run --release --bin kiwi -- --config config.example.toml`
- Cluster-style startup example from README:
  - `cargo run --release -- --config cluster.conf --init-cluster`
- Run all tests: `cargo test`
- Run one package’s tests: `cargo test -p storage`
- Run one test by name: `cargo test test_redis_mset`
- Run Python integration tests:
  - `pytest tests/python/ -v`
  - `python tests/python/test_mset.py`
- Format and lint before finishing significant Rust changes:
  - `cargo fmt --all`
  - `cargo clippy --all-features --workspace -- -D warnings`

## Repository shape

This is a Rust workspace. The main crates are:

- `src/server`: binary entrypoint and process startup
- `src/net`: TCP server, RESP parsing, connection handling, and bridge to storage runtime
- `src/cmd`: Redis-compatible command definitions and command table
- `src/executor`: async worker-pool command executor
- `src/storage`: RocksDB-backed storage engine and Redis data structure implementations
- `src/raft`: OpenRaft integration, Raft node, transport, and HTTP APIs
- `src/conf`: configuration parsing and shared config/raft types
- `src/client`: connection abstraction used by command execution
- `src/resp`: RESP protocol encode/decode
- `src/common/runtime`: dual-runtime orchestration for network and storage work

## Big-picture architecture

### Request path

The normal request flow is:

1. `src/server/src/main.rs` starts `RuntimeManager`, opens storage, initializes storage components, and creates the TCP server.
2. `src/net/src/lib.rs` builds a `NetworkServer` through `ServerFactory::create_server`.
3. `src/net/src/network_server.rs` accepts TCP connections and hands each connection to the network-side handler.
4. `src/net` parses RESP requests, looks up command metadata from the command table, and executes through the executor.
5. `src/cmd` command implementations operate on `storage::storage::Storage`.
6. `src/storage` maps Redis operations onto RocksDB-backed internal formats.

When tracing a bug, start at `src/server/src/main.rs`, then follow into `src/net`, `src/cmd`, and finally `src/storage`.

### Dual runtime model

Kiwi is organized around separate network and storage runtimes for isolation. This is visible in `README.md`, the `runtime` crate dependency wiring, and the server bootstrap in `src/server/src/main.rs`.

Important implications:

- Server startup first creates and starts `RuntimeManager`.
- Storage components are initialized before the TCP server is created.
- Network code should use the storage client/runtime bridge rather than assuming direct single-threaded execution.
- There is still older/legacy code in places; prefer the dual-runtime path when making changes.

### Command system

The command layer is table-driven.

- `src/cmd/src/lib.rs` defines the `Cmd` trait, command metadata, flags, and argument validation.
- `src/cmd/src/table.rs` builds the registry used by the network layer.
- Most Redis commands are implemented as one file per command under `src/cmd/src/`.

If adding or changing a command, check:

- the command’s own file
- the command table registration
- whether flags/arity/RAFT behavior need updating

### Storage model

`src/storage` is not a thin wrapper around RocksDB; it implements Redis-like semantics with custom key/value encodings and multiple logical data models.

Key points:

- `src/storage/src/storage.rs` owns top-level storage state, background tasks, TTL cleanup, slot indexing, and multiple Redis instances.
- `Storage::open()` creates one RocksDB-backed `Redis` instance per configured DB instance.
- Data types are split across specialized modules like `redis_strings`, `redis_hashes`, `redis_lists`, `redis_sets`, and `redis_zsets`.
- Encodings and key layout live in modules such as `base_*_format.rs`, `lists_data_key_format.rs`, and related format files.
- Column family naming matters and is mirrored in both storage and raft code.

For data correctness bugs, understand the on-disk encoding module before changing higher-level command logic.

### Raft integration status

This repo contains substantial Raft/OpenRaft code under `src/raft` and shared raft types in `src/conf`, including:

- node creation in `src/raft/src/node.rs`
- HTTP API handlers in `src/raft/src/api.rs`
- log store, state machine, and network transport in `src/raft/src/*`

Cluster mode IS implemented and can be enabled via config file + `--init-cluster` flag. The server runs in single-node mode by default, but cluster mode is available when explicitly configured. See `docs/raft/OPENRAFT_INTEGRATION.md` for integration details.

### Configuration

Configuration parsing lives in `src/conf/src/config.rs`.

Notable details:

- Defaults bind to `127.0.0.1:7379`.
- The config parser accepts Redis-style config text rather than only standard TOML.
- Config includes Raft-related fields for cluster mode.
- RocksDB tuning options are configured through `Config::get_rocksdb_options()`.
- Cluster mode: use `--config <file> --init-cluster` to start a cluster node.

## Testing notes

- Rust tests are spread across workspace crates; use package-specific `cargo test -p <crate>` when iterating.
- There are Python integration tests under `tests/python/` that expect a running Kiwi server.
- `tests/README.md` documents Python tests and also describes some planned-but-not-fully-present test areas; verify current files before relying on it.
- Test organization:
  - Unit tests: in each crate's `tests/` directory (e.g., `src/storage/tests/`)
  - Python integration tests: `tests/python/`
  - Raft network partition tests: `tests/raft_network_partition_tests.rs`
- The repo has a large `target/` directory in the working tree; avoid searching it when grepping for source code.

## Practical guidance for future Claude instances

- Prefer searching inside `src/` and `tests/` instead of the whole repo to avoid `target/` noise.
- For server behavior, check `src/server/src/main.rs` for the actual startup flow.
- For command work, inspect both the command file and `src/cmd/src/table.rs`.
- For storage bugs, inspect the corresponding Redis data-type module plus the relevant key/value format modules.
- For Raft/cluster work, refer to `docs/raft/OPENRAFT_INTEGRATION.md` and verify current runtime behavior.
- Dual-runtime implications: network and storage runtimes are separate; storage operations go through the runtime bridge.