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

//! LogIndex type definitions

/// LogIndex type alias (i64 to match RocksDB conventions)
pub type LogIndex = i64;

/// Sequence number type alias
pub type SequenceNumber = u64;

/// Pair of (log_index, seqno) for tracking applied log state
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LogIndexSeqnoPair {
    log_index: LogIndex,
    seqno: SequenceNumber,
}

impl LogIndexSeqnoPair {
    pub fn new(log_index: LogIndex, seqno: SequenceNumber) -> Self {
        Self { log_index, seqno }
    }

    pub fn log_index(&self) -> LogIndex {
        self.log_index
    }

    pub fn seqno(&self) -> SequenceNumber {
        self.seqno
    }

    pub fn set(&mut self, log_index: LogIndex, seqno: SequenceNumber) {
        self.log_index = log_index;
        self.seqno = seqno;
    }

    /// Compare seqno with another pair
    pub fn ge_seqno(&self, other: &Self) -> bool {
        self.seqno >= other.seqno
    }
}

/// Pair of (log_index, seqno) for tracking applied log state (applied vs flushed)
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LogIndexAndSequencePair {
    applied_log_index: LogIndex,
    seqno: SequenceNumber,
}

impl LogIndexAndSequencePair {
    pub fn new(applied_log_index: LogIndex, seqno: SequenceNumber) -> Self {
        Self {
            applied_log_index,
            seqno,
        }
    }

    pub fn applied_log_index(&self) -> LogIndex {
        self.applied_log_index
    }

    pub fn seqno(&self) -> SequenceNumber {
        self.seqno
    }
}
