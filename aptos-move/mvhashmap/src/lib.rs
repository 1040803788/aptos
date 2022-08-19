// Copyright (c) Aptos
// SPDX-License-Identifier: Apache-2.0

use aptos_aggregator::delta_change_set::DeltaOp;
use aptos_types::write_set::DeserializeU128;
use crossbeam::utils::CachePadded;
use dashmap::DashMap;
use std::{
    collections::btree_map::BTreeMap,
    hash::Hash,
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    },
};

#[cfg(test)]
mod unit_tests;

// TODO: re-use definitions with the scheduler.
pub type TxnIndex = usize;
pub type Incarnation = usize;
pub type Version = (TxnIndex, Incarnation);

const FLAG_DONE: usize = 0;
const FLAG_ESTIMATE: usize = 1;

/// Every entry in shared multi-version data-structure has an "estimate" flag
/// and some content.
struct Entry<V> {
    /// Used to mark the entry as a "write estimate".
    flag: AtomicUsize,
    /// Actual content.
    cell: EntryCell<V>,
}

/// Represents the content of a single entry in multi-version data-structure.
enum EntryCell<V> {
    /// Recorded in the shared multi-version data-structure for each write. It
    /// has: 1) Incarnation number of the transaction that wrote the entry (note
    /// that TxnIndex is part of the key and not recorded here), 2) actual data
    /// stored in a shared pointer (to ensure ownership and avoid clones).
    Write(Incarnation, Arc<V>),
    /// Recorded in the shared multi-version data-structure for each delta.
    Delta(DeltaOp),
}

impl<V> Entry<V> {
    pub fn new_write_from(flag: usize, incarnation: Incarnation, data: V) -> Entry<V> {
        Entry {
            flag: AtomicUsize::new(flag),
            cell: EntryCell::Write(incarnation, Arc::new(data)),
        }
    }

    pub fn new_delta_from(flag: usize, data: DeltaOp) -> Entry<V> {
        Entry {
            flag: AtomicUsize::new(flag),
            cell: EntryCell::Delta(data),
        }
    }

    pub fn flag(&self) -> usize {
        self.flag.load(Ordering::SeqCst)
    }

    pub fn mark_estimate(&self) {
        self.flag.store(FLAG_ESTIMATE, Ordering::SeqCst);
    }
}

/// Main multi-version data-structure used by threads to read/write during parallel
/// execution. Maps each access path to an interal BTreeMap that contains the indices
/// of transactions that write at the given access path alongside the corresponding
/// entries of WriteCell type.
///
/// Concurrency is managed by DashMap, i.e. when a method accesses a BTreeMap at a
/// given key, it holds exclusive access and doesn't need to explicitly synchronize
/// with other reader/writers.
pub struct MVHashMap<K, V> {
    data: DashMap<K, BTreeMap<TxnIndex, CachePadded<Entry<V>>>>,
}

/// Returned as Err(..) when failed to read from the multi-version data-structure.
#[derive(Debug, PartialEq)]
pub enum MVHashMapError {
    /// No prior entry is found.
    NotFound,
    /// Read resulted in an unresolved delta value.
    Unresolved(DeltaOp),
    /// A dependency on other transaction has been found during the read.
    Dependency(TxnIndex),
}

/// Returned as Ok(..) when read successfully from the multi-version data-structure.
#[derive(Debug, PartialEq)]
pub enum MVHashMapOutput<V> {
    /// Result of resolved delta op, always u128. Unlike with `Version`, we return
    /// actual data because u128 is cheap to copy amd validation can be done correctly
    /// on values as well (ABA is not a problem).
    Resolved(u128),
    /// Information from the last versioned-write. Note that the version is returned
    /// and not the data to avoid passing big values around.
    Version(Version, Arc<V>),
}

pub type Result<V> = anyhow::Result<MVHashMapOutput<V>, MVHashMapError>;

impl<K: Hash + Clone + Eq, V: DeserializeU128> MVHashMap<K, V> {
    pub fn new() -> MVHashMap<K, V> {
        MVHashMap {
            data: DashMap::new(),
        }
    }

    /// Add a write of versioned data at a specified key. If the entry is overwritten, asserts
    /// that the new incarnation is strictly higher.
    pub fn add_write(&self, key: &K, version: Version, data: V) {
        let (txn_idx, incarnation) = version;

        let mut map = self.data.entry(key.clone()).or_insert(BTreeMap::new());
        let prev_entry = map.insert(
            txn_idx,
            CachePadded::new(Entry::new_write_from(FLAG_DONE, incarnation, data)),
        );

        // Assert that the previous entry for txn_idx, if present, had lower incarnation.
        assert!(prev_entry
            .map(|entry| matches!(entry.cell, EntryCell::Write(i, _) if i < incarnation))
            .unwrap_or(true));
    }

    /// Add a delta at a specified key.
    pub fn add_delta(&self, key: &K, txn_idx: usize, delta: DeltaOp) {
        let mut map = self.data.entry(key.clone()).or_insert(BTreeMap::new());
        map.insert(
            txn_idx,
            CachePadded::new(Entry::new_delta_from(FLAG_DONE, delta)),
        );
    }

    /// Mark an entry from transaction 'txn_idx' at access path 'key' as an estimated write
    /// (for future incarnation). Will panic if the entry is not in the data-structure.
    pub fn mark_estimate(&self, key: &K, txn_idx: TxnIndex) {
        let map = self.data.get(key).expect("Path must exist");
        map.get(&txn_idx)
            .expect("Entry by txn must exist")
            .mark_estimate();
    }

    /// Delete an entry from transaction 'txn_idx' at access path 'key'. Will panic
    /// if the access path has never been written before.
    pub fn delete(&self, key: &K, txn_idx: TxnIndex) {
        // TODO: investigate logical deletion.
        let mut map = self.data.get_mut(key).expect("Path must exist");
        map.remove(&txn_idx);
    }

    /// Read entry from transaction 'txn_idx' at access path 'key'.
    pub fn read(&self, key: &K, txn_idx: TxnIndex) -> Result<V> {
        use MVHashMapError::*;
        use MVHashMapOutput::*;

        match self.data.get(key) {
            Some(tree) => {
                let mut iter = tree.range(0..txn_idx);

                // If read encounters a delta, it must traverse the block of
                // transactions (top-down) until it encounters a write or reaches
                // the end of the block. During traversal, all deltas have to be
                // aggregated together.
                let mut aggregator: Option<DeltaOp> = None;
                while let Some((idx, entry)) = iter.next_back() {
                    let flag = entry.flag();

                    if flag == FLAG_ESTIMATE {
                        // Found a dependency.
                        return Err(Dependency(*idx));
                    } else {
                        // The entry should be populated.
                        debug_assert!(flag == FLAG_DONE);

                        match &entry.cell {
                            EntryCell::Write(incarnation, data) => {
                                match aggregator.as_ref() {
                                    Some(delta) => {
                                        // Read hit a write during traversal. We need
                                        // to deserialize the value of the write and
                                        // apply the aggregated delta.
                                        // TODO: we do not support error at the moment,
                                        // so if delta application fails, we panic.
                                        let value = data
                                            .deserialize()
                                            .expect("cannot deserialize into u128");
                                        let result = delta.apply_to(value).expect(
                                            "delta application fails but it shouldn't haves",
                                        );
                                        return Ok(Resolved(result));
                                    }
                                    None => {
                                        // Read hit a write without any traversal
                                        // or delta aggregation. In this case, return
                                        // the version.
                                        let write_version = (*idx, *incarnation);
                                        return Ok(Version(write_version, data.clone()));
                                    }
                                }
                            }
                            EntryCell::Delta(delta) => {
                                match aggregator.as_mut() {
                                    Some(other_delta) => {
                                        // Read hit a delta during traversing the
                                        // block and aggregating other deltas. Merge
                                        // two deltas together.
                                        // TODO: merging deltas can also result in
                                        // error. Once again, there is nothing we can
                                        // do at the moment, so panic if this happens.

                                        // TODO: We need a function:
                                        //   delta.merge(other_delta)
                                        // Wait until other PR lands to reuse that. For
                                        // now place anything.

                                        // LINE BELOW MAKES NO SENSE!
                                        other_delta.apply_to(0);
                                    }
                                    None => {
                                        // Read hit a delta value and has to
                                        // start data aggregation. Initialize the
                                        // accumulator and continue traversal.
                                        aggregator = Some(delta.clone())
                                    }
                                }
                            }
                        }
                    }
                }

                // It can happen that while traversing the block and resolving
                // deltas the actual written value has not been seen yet (i.e.
                // it is not added as an entry to the data-structure).
                match aggregator {
                    Some(delta) => Err(Unresolved(delta)),
                    None => Err(NotFound),
                }
            }
            None => Err(NotFound),
        }
    }
}
