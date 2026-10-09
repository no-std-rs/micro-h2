//! The HPACK dynamic table.
//!
//! # Why this cannot be skipped
//!
//! It is tempting to ignore: a client could decline to *use* the dynamic table
//! in the headers it sends, and never add an entry of its own. But the table is
//! shared state built from the peer's stream, and the peer decides what goes in
//! it. Every "literal with incremental indexing" the server sends inserts an
//! entry, and every later reference is an index into a table that only exists if
//! we kept it.
//!
//! Skipping an insertion does not produce a missing header — it shifts every
//! subsequent index by one, so headers decode as *other headers*, plausibly and
//! silently, for the rest of the connection. That is why decoding an unknown
//! index is an error here rather than a best-effort guess.
//!
//! # Shape
//!
//! A ring of fixed capacity. Entry 62 is the newest, and inserting evicts from
//! the far end — which is the opposite of what a naive `Vec` push does, and the
//! reason indices are computed rather than stored.

use crate::Error;
use crate::hpack::static_table::DYNAMIC_BASE;

/// Fixed number of entries. This represents every entry that fits the
/// connection's advertised 256-byte table (32 bytes of overhead per entry).
pub const MAX_ENTRIES: usize = 8;

/// A single field can use all 256 advertised bytes minus entry overhead.
pub const MAX_ENTRY_LEN: usize = 224;

/// RFC 7541 section 4.1: an entry's size is its name plus its value plus 32
/// bytes of assumed overhead. The constant is part of the wire protocol — both
/// ends must compute the same size or they evict at different moments and the
/// tables diverge.
const ENTRY_OVERHEAD: usize = 32;

#[derive(Clone)]
struct Entry {
    name: heapless::String<MAX_ENTRY_LEN>,
    value: heapless::String<MAX_ENTRY_LEN>,
}

impl Entry {
    fn size(&self) -> usize {
        self.name.len() + self.value.len() + ENTRY_OVERHEAD
    }
}

/// The decoder's view of the table the peer is building.
#[derive(Clone)]
pub struct DynamicTable {
    /// Newest first, so index 62 is `entries[0]`.
    entries: heapless::Deque<Entry, MAX_ENTRIES>,
    size: usize,
    capacity: usize,
    synchronized: bool,
}

impl DynamicTable {
    pub fn new(capacity: usize) -> Self {
        Self {
            entries: heapless::Deque::new(),
            size: 0,
            capacity,
            synchronized: true,
        }
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    pub fn size(&self) -> usize {
        self.size
    }

    pub fn capacity(&self) -> usize {
        self.capacity
    }

    /// Apply a dynamic table size update from the peer.
    ///
    /// Shrinking evicts immediately; the peer has already done the same, so the
    /// two tables stay in step.
    pub fn set_capacity(&mut self, capacity: usize) {
        self.capacity = capacity;
        self.evict_to_fit(0);
        if capacity == 0 {
            self.synchronized = true;
        }
    }

    pub(crate) fn synchronized(&self) -> bool {
        self.synchronized
    }

    /// Look up a 1-based HPACK index, which for the dynamic table starts at 62.
    pub fn get(&self, index: usize) -> Option<(&str, &str)> {
        if !self.synchronized {
            return None;
        }
        let offset = index.checked_sub(DYNAMIC_BASE)?;
        self.entries
            .iter()
            .nth(offset)
            .map(|e| (e.name.as_str(), e.value.as_str()))
    }

    /// Insert an indexed header, evicting only as required by the peer's byte
    /// capacity. A capacity above our fixed storage is supported only while its
    /// entries fit. Exceeding storage invalidates dynamic lookups until an
    /// explicit table clear; silently evicting would shift the peer's indices.
    pub fn insert(&mut self, name: &str, value: &str) {
        let size = name.len() + value.len() + ENTRY_OVERHEAD;
        self.evict_to_fit(size);
        if size > self.capacity {
            self.synchronized = true;
            return;
        }
        if !self.synchronized
            || name.len() > MAX_ENTRY_LEN
            || value.len() > MAX_ENTRY_LEN
            || self.entries.is_full()
        {
            self.synchronized = false;
            return;
        }
        let (Ok(name), Ok(value)) = (
            heapless::String::try_from(name),
            heapless::String::try_from(value),
        ) else {
            return;
        };
        let entry = Entry { name, value };
        self.size += entry.size();
        let _ = self.entries.push_front(entry);
    }

    fn evict_to_fit(&mut self, incoming: usize) {
        while self.size + incoming > self.capacity && !self.entries.is_empty() {
            self.evict_oldest();
        }
    }

    fn evict_oldest(&mut self) {
        if let Some(entry) = self.entries.pop_back() {
            self.size -= entry.size();
        }
    }
}

/// Resolve an index against the static table first, then the dynamic one.
pub fn lookup(table: &DynamicTable, index: usize) -> Result<(&str, &str), Error> {
    if index < DYNAMIC_BASE {
        return crate::hpack::static_table::get(index).ok_or(Error::Hpack);
    }
    table.get(index).ok_or(Error::Hpack)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_newest_entry_is_index_62() {
        let mut table = DynamicTable::new(4096);
        table.insert("first", "1");
        assert_eq!(table.get(62), Some(("first", "1")));

        // Inserting pushes the previous entry *up* an index, which is the whole
        // reason indices are computed rather than stored.
        table.insert("second", "2");
        assert_eq!(table.get(62), Some(("second", "2")));
        assert_eq!(table.get(63), Some(("first", "1")));
        assert_eq!(table.get(64), None);
    }

    #[test]
    fn entry_size_includes_the_thirty_two_byte_overhead() {
        // Both ends must compute this identically or they evict at different
        // moments and every index after that diverges.
        let mut table = DynamicTable::new(4096);
        table.insert("ab", "cd");
        assert_eq!(table.size(), 2 + 2 + 32);
    }

    #[test]
    fn a_full_table_evicts_the_oldest_first() {
        // Capacity for exactly two of these.
        let mut table = DynamicTable::new((1 + 1 + 32) * 2);
        table.insert("a", "1");
        table.insert("b", "2");
        assert_eq!(table.len(), 2);

        table.insert("c", "3");
        assert_eq!(table.len(), 2);
        assert_eq!(table.get(62), Some(("c", "3")));
        assert_eq!(table.get(63), Some(("b", "2")));
        assert_eq!(table.get(64), None, "the oldest entry must be gone");
    }

    #[test]
    fn an_entry_larger_than_the_table_empties_it_and_is_dropped() {
        // RFC 7541 section 4.4. Getting this wrong leaves a phantom entry and
        // shifts every later index.
        let mut table = DynamicTable::new(64);
        table.insert("a", "1");
        assert_eq!(table.len(), 1);

        let mut huge = heapless::String::<128>::new();
        for _ in 0..100 {
            huge.push('x').unwrap();
        }
        table.insert("enormous", &huge);
        assert!(table.is_empty());
        assert_eq!(table.get(62), None);
    }

    #[test]
    fn shrinking_the_capacity_evicts_immediately() {
        let mut table = DynamicTable::new(4096);
        table.insert("a", "1");
        table.insert("b", "2");
        table.set_capacity(34);
        assert_eq!(table.len(), 1);
        assert_eq!(table.get(62), Some(("b", "2")));
        table.set_capacity(0);
        assert!(table.is_empty());
    }

    #[test]
    fn lookup_spans_both_tables_and_refuses_what_it_cannot_resolve() {
        let mut table = DynamicTable::new(4096);
        table.insert("custom", "value");
        assert_eq!(lookup(&table, 2).unwrap(), (":method", "GET"));
        assert_eq!(lookup(&table, 62).unwrap(), ("custom", "value"));
        // An index nobody defined must be an error. Guessing would decode later
        // headers as different headers, plausibly and silently.
        assert_eq!(lookup(&table, 63), Err(Error::Hpack));
        assert_eq!(lookup(&table, 0), Err(Error::Hpack));
    }

    #[test]
    fn entry_count_exhaustion_disables_dynamic_lookups() {
        // Exceeding fixed storage must fail closed, without inventing peer
        // evictions that change the meaning of a dynamic index.
        let mut table = DynamicTable::new(1_000_000);
        for i in 0..MAX_ENTRIES + 10 {
            let mut value = heapless::String::<8>::new();
            let _ = core::fmt::Write::write_fmt(&mut value, format_args!("{i}"));
            table.insert("k", &value);
        }
        assert_eq!(table.len(), MAX_ENTRIES);
        assert_eq!(table.get(62), None);
    }
}
