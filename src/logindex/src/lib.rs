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
// See the License for the language governing permissions and
// limitations under the License.

pub mod cf_tracker;
pub mod collector;
pub mod db_access;
pub mod event_listener;
pub mod table_properties;
pub mod types;

pub use cf_tracker::{LogIndexOfColumnFamilies, SmallestIndexRes};
pub use collector::LogIndexAndSequenceCollector;
pub use event_listener::LogIndexAndSequenceCollectorPurger;
pub use table_properties::{
    LogIndexTablePropertiesCollectorFactory, PROPERTY_KEY, get_largest_log_index_from_collection,
    read_stats_from_table_props,
};
pub use types::{LogIndex, LogIndexAndSequencePair, LogIndexSeqnoPair, SequenceNumber};

/// Number of column families, consistent with storage::ColumnFamilyIndex::COUNT
pub const COLUMN_FAMILY_COUNT: usize = storage::ColumnFamilyIndex::COUNT;

/// List of CF names, in the same order as storage::ColumnFamilyIndex
pub const CF_NAMES: [&str; COLUMN_FAMILY_COUNT] = [
    "default",       // MetaCF = 0
    "hash_data_cf",  // HashesDataCF = 1
    "set_data_cf",   // SetsDataCF = 2
    "list_data_cf",  // ListsDataCF = 3
    "zset_data_cf",  // ZsetsDataCF = 4
    "zset_score_cf", // ZsetsScoreCF = 5
];

/// Convert CF name to index
pub fn cf_name_to_index(name: &[u8]) -> Option<usize> {
    CF_NAMES.iter().position(|n| n.as_bytes() == name)
}
