// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 The Hyperlight Authors.

//! Snapshot memory model:
//!
//! Snapshot
//! `-- memory: Arc<SnapshotMemory>
//!     |-- layers: Box<[SnapshotLayer]>
//!     |   |-- SnapshotLayer 0
//!     |   |   |-- blob: Arc<SnapshotBlob>
//!     |   |   |   |-- memory: ReadonlySharedMemory
//!     |   |   |   `-- data_gpa_range: SnapshotDataRange
//!     |   |   `-- live_data_ranges: Box<[Range<usize>]>
//!     |   `-- SnapshotLayer 1
//!     |       `-- ...
//!     `-- page_tables: Arc<SnapshotPageTables>

use std::ops::Range;
use std::sync::Arc;

use hyperlight_common::vmem::PAGE_SIZE;

use crate::Result;
use crate::mem::layout::SandboxMemoryLayout;
use crate::mem::shared_mem::{ReadonlySharedMemory, SharedMemory};

mod backing;
pub(crate) use backing::SnapshotMemoryBacking;

// Arbitrary cap on the mappings of one snapshot. Each mapping costs a
// map/unmap when a sandbox switches snapshots, so bounding this bounds restore
// latency. Every layer contributes at least one mapping, so this bounds the
// layer count too.
pub(crate) const MAX_SNAPSHOT_MAPPINGS: usize = 30;
pub(crate) const MAX_SNAPSHOT_RETAINED_BYTES: usize =
    SandboxMemoryLayout::MAX_MEMORY_SIZE.saturating_mul(2);

/// Memory state assembled from immutable snapshot layers.
///
/// Layer ranges never overlap.
#[derive(Debug)]
pub(crate) struct SnapshotMemory {
    /// Layers sorted by the start GPA of each blob's data range, which lookups
    /// binary search.
    layers: Box<[SnapshotLayer]>,
    /// The complete page-table tree. Restore copies the tree into scratch
    /// memory.
    page_tables: Arc<SnapshotPageTables>,
}

/// One layer of a snapshot, backed by a single blob. The live ranges are the
/// parts of the blob's guest address range this layer contributes, and other
/// layers hold the rest. The blob is shared with any other snapshot that reuses
/// it, each with its own live ranges.
#[derive(Clone, Debug)]
pub(crate) struct SnapshotLayer {
    blob: Arc<SnapshotBlob>,
    /// Blob-relative byte ranges, sorted by start offset, disjoint, and
    /// coalesced.
    live_data_ranges: Box<[Range<usize>]>,
}

/// Immutable storage containing guest data. A snapshot references a blob at
/// most once.
#[derive(Debug)]
pub(crate) struct SnapshotBlob {
    memory: ReadonlySharedMemory,
    /// Guest physical range represented by the equally sized prefix of `memory`.
    data_gpa_range: SnapshotDataRange,
}

/// The page tables one snapshot restores into scratch memory. `memory` is
/// padded to a host page, so `len` gives the tree itself.
#[derive(Debug)]
pub(crate) struct SnapshotPageTables {
    memory: ReadonlySharedMemory,
    len: usize,
}

/// Guest physical address range covered by a blob's guest-addressable data.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct SnapshotDataRange(Range<u64>);

impl SnapshotDataRange {
    pub(crate) fn gpa_start(&self) -> u64 {
        self.0.start
    }

    pub(crate) fn len(&self) -> usize {
        usize::try_from(self.0.end - self.0.start)
            .expect("snapshot data range length was constructed from usize")
    }

    pub(crate) fn gpa_range(&self) -> Range<u64> {
        self.0.clone()
    }
}

impl SnapshotBlob {
    pub(crate) fn new(
        memory: ReadonlySharedMemory,
        data_gpa_start: u64,
        data_len: usize,
        scratch_base_gpa: u64,
    ) -> Result<Self> {
        let data_gpa_range = validate_snapshot_blob_layout(
            memory.mem_size(),
            data_gpa_start,
            data_len,
            scratch_base_gpa,
            page_size::get(),
        )?;

        Ok(Self {
            memory,
            data_gpa_range,
        })
    }

    pub(crate) fn memory(&self) -> &ReadonlySharedMemory {
        &self.memory
    }

    pub(crate) fn data_gpa_range(&self) -> &SnapshotDataRange {
        &self.data_gpa_range
    }

    fn memory_len(&self) -> usize {
        self.memory.mem_size()
    }
}

impl SnapshotPageTables {
    pub(crate) fn new(memory: ReadonlySharedMemory, len: usize) -> Result<Self> {
        validate_snapshot_page_tables(memory.mem_size(), len, page_size::get())?;
        Ok(Self { memory, len })
    }

    pub(crate) fn bytes(&self) -> &[u8] {
        &self.memory.as_slice()[..self.len]
    }

    pub(crate) fn storage_bytes(&self) -> &[u8] {
        self.memory.as_slice()
    }

    pub(crate) fn len(&self) -> usize {
        self.len
    }
}

impl SnapshotLayer {
    pub(crate) fn new(
        blob: Arc<SnapshotBlob>,
        live_data_ranges: Box<[Range<usize>]>,
    ) -> Result<Self> {
        validate_snapshot_live_data(
            blob.data_gpa_range.len(),
            live_data_ranges.iter().cloned(),
            page_size::get(),
        )?;

        Ok(Self {
            blob,
            live_data_ranges,
        })
    }

    pub(crate) fn blob(&self) -> &Arc<SnapshotBlob> {
        &self.blob
    }

    pub(crate) fn live_data_ranges(&self) -> &[Range<usize>] {
        &self.live_data_ranges
    }

    pub(crate) fn resolve(&self, gpa: u64, len: usize) -> Option<usize> {
        let data = self.blob.data_gpa_range();
        let relative = gpa.checked_sub(data.gpa_start())?;
        let offset = usize::try_from(relative).ok()?;
        let end = offset.checked_add(len)?;
        if end > data.len() {
            return None;
        }
        self.live_data_ranges
            .iter()
            .any(|range| range.start <= offset && end <= range.end)
            .then_some(offset)
    }

    fn resolve_live_chunk(&self, gpa: u64) -> Option<(usize, usize)> {
        let data = self.blob.data_gpa_range();
        let offset = usize::try_from(gpa.checked_sub(data.gpa_start())?).ok()?;
        let range_index = self
            .live_data_ranges
            .partition_point(|range| range.start <= offset)
            .checked_sub(1)?;
        let range = &self.live_data_ranges[range_index];
        (offset < range.end).then(|| (offset, range.end - offset))
    }
}

impl SnapshotMemory {
    pub(crate) fn new(
        mut layers: Box<[SnapshotLayer]>,
        page_tables: Arc<SnapshotPageTables>,
    ) -> Result<Self> {
        validate_snapshot_layer_count(layers.len())?;
        layers.sort_by_key(|layer| layer.blob.data_gpa_range.gpa_start());

        let mut mapping_count = 0usize;
        let mut mapped_bytes = 0usize;
        let mut retained_bytes = page_tables.memory.mem_size();
        for layer in layers.iter() {
            mapping_count = mapping_count
                .checked_add(layer.live_data_ranges.len())
                .ok_or_else(|| crate::new_error!("snapshot mapping count overflows"))?;
            for range in &layer.live_data_ranges {
                mapped_bytes = mapped_bytes
                    .checked_add(range.end - range.start)
                    .ok_or_else(|| crate::new_error!("snapshot mapped byte count overflows"))?;
            }
            retained_bytes = retained_bytes
                .checked_add(layer.blob.memory_len())
                .ok_or_else(|| crate::new_error!("snapshot retained byte count overflows"))?;
        }
        // The mapping cap bounds the layer count, so the quadratic scan below
        // runs on a bounded list.
        validate_snapshot_totals(mapping_count, mapped_bytes, retained_bytes)?;
        for (index, layer) in layers.iter().enumerate() {
            if layers[..index]
                .iter()
                .any(|other| Arc::ptr_eq(&other.blob, &layer.blob))
            {
                return Err(crate::new_error!(
                    "snapshot references the same blob more than once"
                ));
            }
        }
        validate_sorted_snapshot_gpa_ranges(
            layers
                .iter()
                .map(|layer| layer.blob.data_gpa_range.gpa_range()),
        )?;

        Ok(Self {
            layers,
            page_tables,
        })
    }

    pub(crate) fn from_flat(
        memory: ReadonlySharedMemory,
        gpa_start: u64,
        data_len: usize,
        page_tables: Arc<SnapshotPageTables>,
        scratch_base_gpa: u64,
    ) -> Result<Self> {
        let blob = Arc::new(SnapshotBlob::new(
            memory,
            gpa_start,
            data_len,
            scratch_base_gpa,
        )?);
        let layer = SnapshotLayer::new(blob, single_range(0..data_len))?;
        Self::new(Box::new([layer]), page_tables)
    }

    #[cfg(test)]
    pub(crate) fn mem_size(&self) -> usize {
        self.layers[0].blob.memory().mem_size()
    }

    pub(crate) fn layers(&self) -> &[SnapshotLayer] {
        &self.layers
    }

    pub(crate) fn page_tables(&self) -> &Arc<SnapshotPageTables> {
        &self.page_tables
    }

    pub(crate) fn gpa_span_len(&self) -> usize {
        let end = self
            .layers
            .iter()
            .map(|layer| layer.blob.data_gpa_range.0.end)
            .max()
            .unwrap_or(SandboxMemoryLayout::BASE_ADDRESS as u64);
        usize::try_from(end - SandboxMemoryLayout::BASE_ADDRESS as u64)
            .expect("SnapshotMemory validates its GPA span")
    }

    pub(crate) fn page_table_len(&self) -> usize {
        self.page_tables.len()
    }

    pub(crate) fn resolve(&self, gpa: u64, len: usize) -> Option<(usize, usize)> {
        let index = self.layer_for_gpa(gpa)?;
        self.layers[index]
            .resolve(gpa, len)
            .map(|offset| (index, offset))
    }

    pub(crate) fn resolve_live_chunk(&self, gpa: u64) -> Option<(usize, usize, usize)> {
        let index = self.layer_for_gpa(gpa)?;
        self.layers[index]
            .resolve_live_chunk(gpa)
            .map(|(offset, len)| (index, offset, len))
    }

    fn layer_for_gpa(&self, gpa: u64) -> Option<usize> {
        self.layers
            .partition_point(|layer| layer.blob.data_gpa_range.gpa_start() <= gpa)
            .checked_sub(1)
    }

    fn resolve_page_table_range(
        &self,
        pt_gpa_base: u64,
        gpa: u64,
        len: usize,
    ) -> Result<Range<usize>> {
        let offset = usize::try_from(
            gpa.checked_sub(pt_gpa_base)
                .ok_or_else(|| crate::new_error!("page-table GPA is below its base"))?,
        )?;
        let end = offset
            .checked_add(len)
            .ok_or_else(|| crate::new_error!("page-table read range overflows"))?;
        if end > self.page_tables.len() {
            return Err(crate::new_error!("page-table read range is out of bounds"));
        }
        Ok(offset..end)
    }

    pub(crate) fn read_page_tables(
        &self,
        pt_gpa_base: u64,
        gpa: u64,
        destination: &mut [u8],
    ) -> Result<()> {
        let range = self.resolve_page_table_range(pt_gpa_base, gpa, destination.len())?;
        destination.copy_from_slice(&self.page_tables.bytes()[range]);
        Ok(())
    }
}

pub(crate) fn validate_snapshot_blob_layout(
    memory_len: usize,
    data_gpa_start: u64,
    data_len: usize,
    scratch_base_gpa: u64,
    host_page_size: usize,
) -> Result<SnapshotDataRange> {
    if data_len == 0
        || memory_len == 0
        || !memory_len.is_multiple_of(host_page_size)
        || !is_page_aligned(data_len, host_page_size)
        || data_len > memory_len
        || memory_len - data_len >= host_page_size
    {
        return Err(crate::new_error!(
            "snapshot layer storage or data size is invalid"
        ));
    }
    if !is_page_aligned_u64(data_gpa_start, host_page_size) {
        return Err(crate::new_error!("snapshot layer GPA is not aligned"));
    }
    if data_gpa_start < SandboxMemoryLayout::BASE_ADDRESS as u64 {
        return Err(crate::new_error!(
            "snapshot layer starts below BASE_ADDRESS"
        ));
    }
    let gpa_end = data_gpa_start
        .checked_add(u64::try_from(data_len)?)
        .ok_or_else(|| crate::new_error!("snapshot layer GPA range overflows"))?;
    if gpa_end > scratch_base_gpa {
        return Err(crate::new_error!(
            "snapshot layer data overlaps scratch memory"
        ));
    }
    Ok(SnapshotDataRange(data_gpa_start..gpa_end))
}

pub(crate) fn validate_snapshot_page_tables(
    memory_len: usize,
    len: usize,
    host_page_size: usize,
) -> Result<()> {
    if len == 0
        || !len.is_multiple_of(PAGE_SIZE)
        || len > memory_len
        || !memory_len.is_multiple_of(host_page_size)
        || memory_len - len >= host_page_size
    {
        return Err(crate::new_error!("snapshot page-table size is invalid"));
    }
    Ok(())
}

pub(crate) fn validate_snapshot_live_data(
    data_len: usize,
    live_data: impl IntoIterator<Item = Range<usize>>,
    host_page_size: usize,
) -> Result<()> {
    let mut previous_end = None;
    for range in live_data {
        if range.start >= range.end
            || !is_page_aligned(range.start, host_page_size)
            || !is_page_aligned(range.end, host_page_size)
            || range.end > data_len
        {
            return Err(crate::new_error!(
                "snapshot layer has an invalid live-data range"
            ));
        }
        if previous_end.is_some_and(|end| end >= range.start) {
            return Err(crate::new_error!(
                "snapshot live-data ranges are not coalesced"
            ));
        }
        previous_end = Some(range.end);
    }
    if previous_end.is_none() {
        return Err(crate::new_error!("snapshot layer has no live data"));
    }
    Ok(())
}

pub(crate) fn validate_snapshot_layer_count(layer_count: usize) -> Result<()> {
    if layer_count == 0 {
        return Err(crate::new_error!("snapshot has no layers"));
    }
    Ok(())
}

#[cfg(test)]
pub(crate) fn page_tables_from_bytes(bytes: &[u8]) -> Arc<SnapshotPageTables> {
    Arc::new(
        SnapshotPageTables::new(
            ReadonlySharedMemory::from_bytes(bytes).unwrap(),
            bytes.len(),
        )
        .unwrap(),
    )
}

#[cfg(test)]
pub(crate) fn stub_page_tables() -> Arc<SnapshotPageTables> {
    page_tables_from_bytes(&vec![0u8; PAGE_SIZE])
}

pub(crate) fn validate_snapshot_totals(
    mapping_count: usize,
    mapped_bytes: usize,
    retained_bytes: usize,
) -> Result<()> {
    if mapping_count > MAX_SNAPSHOT_MAPPINGS {
        return Err(crate::new_error!(
            "snapshot mapping count {} exceeds {}",
            mapping_count,
            MAX_SNAPSHOT_MAPPINGS
        ));
    }
    if mapped_bytes > SandboxMemoryLayout::MAX_MEMORY_SIZE {
        return Err(crate::new_error!(
            "snapshot mapped byte count {} exceeds {}",
            mapped_bytes,
            SandboxMemoryLayout::MAX_MEMORY_SIZE
        ));
    }
    if retained_bytes > MAX_SNAPSHOT_RETAINED_BYTES {
        return Err(crate::new_error!(
            "snapshot retained byte count {} exceeds {}",
            retained_bytes,
            MAX_SNAPSHOT_RETAINED_BYTES
        ));
    }
    Ok(())
}

pub(crate) fn validate_sorted_snapshot_gpa_ranges(
    ranges: impl IntoIterator<Item = Range<u64>>,
) -> Result<()> {
    let mut previous_end = None;
    for range in ranges {
        if previous_end.is_some_and(|end| end > range.start) {
            return Err(crate::new_error!("snapshot blob GPA ranges overlap"));
        }
        previous_end = Some(range.end);
    }
    Ok(())
}

fn is_page_aligned(value: usize, host_page_size: usize) -> bool {
    value.is_multiple_of(PAGE_SIZE) && value.is_multiple_of(host_page_size)
}

fn is_page_aligned_u64(value: u64, host_page_size: usize) -> bool {
    value.is_multiple_of(PAGE_SIZE as u64) && value.is_multiple_of(host_page_size as u64)
}

fn single_range(range: Range<usize>) -> Box<[Range<usize>]> {
    std::iter::once(range).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mem::shared_mem::ExclusiveSharedMemory;

    const SCRATCH_BASE_GPA: u64 = 0x1_0000_0000;

    fn blob(data_pages: usize, gpa_start: u64) -> Arc<SnapshotBlob> {
        let data_len = data_pages * PAGE_SIZE;
        let storage = ExclusiveSharedMemory::new(data_len)
            .unwrap()
            .freeze()
            .unwrap();
        Arc::new(SnapshotBlob::new(storage, gpa_start, data_len, SCRATCH_BASE_GPA).unwrap())
    }

    #[test]
    fn flat_snapshot_memory_is_valid() {
        let data_len = 2 * PAGE_SIZE;
        let storage = ExclusiveSharedMemory::new(data_len)
            .unwrap()
            .freeze()
            .unwrap();

        let memory = SnapshotMemory::from_flat(
            storage,
            SandboxMemoryLayout::BASE_ADDRESS as u64,
            data_len,
            stub_page_tables(),
            SCRATCH_BASE_GPA,
        )
        .unwrap();

        assert_eq!(memory.mem_size(), data_len);
        assert_eq!(memory.page_table_len(), PAGE_SIZE);
    }

    #[test]
    fn blob_rejects_data_past_storage() {
        let storage = ExclusiveSharedMemory::new(PAGE_SIZE)
            .unwrap()
            .freeze()
            .unwrap();

        assert!(
            SnapshotBlob::new(
                storage,
                SandboxMemoryLayout::BASE_ADDRESS as u64,
                2 * PAGE_SIZE,
                SCRATCH_BASE_GPA,
            )
            .is_err()
        );
    }

    #[test]
    fn blob_rejects_overflowing_gpa_range() {
        let storage = ExclusiveSharedMemory::new(PAGE_SIZE)
            .unwrap()
            .freeze()
            .unwrap();
        let gpa_start = u64::MAX - PAGE_SIZE as u64 + 1;

        assert!(SnapshotBlob::new(storage, gpa_start, PAGE_SIZE, u64::MAX).is_err());
    }

    #[test]
    fn page_tables_reject_zero_length() {
        let storage = ExclusiveSharedMemory::new(PAGE_SIZE)
            .unwrap()
            .freeze()
            .unwrap();

        assert!(SnapshotPageTables::new(storage, 0).is_err());
    }

    #[test]
    fn page_tables_are_independent_of_layers() {
        let base = SandboxMemoryLayout::BASE_ADDRESS as u64;
        let layer = SnapshotLayer::new(blob(1, base), single_range(0..PAGE_SIZE)).unwrap();
        let page_tables = stub_page_tables();
        let memory = SnapshotMemory::new(Box::new([layer]), page_tables.clone()).unwrap();

        assert!(Arc::ptr_eq(memory.page_tables(), &page_tables));
    }

    #[test]
    fn reads_separate_page_tables() {
        let base = SandboxMemoryLayout::BASE_ADDRESS as u64;
        let layer = SnapshotLayer::new(blob(1, base), single_range(0..PAGE_SIZE)).unwrap();
        let page_tables = page_tables_from_bytes(&vec![0x33; PAGE_SIZE]);
        let memory = SnapshotMemory::new(Box::new([layer]), page_tables).unwrap();
        let mut byte = [0u8; 1];

        memory.read_page_tables(0x8000, 0x8fff, &mut byte).unwrap();

        assert_eq!(byte, [0x33]);
        assert_eq!(memory.resolve(base, PAGE_SIZE), Some((0, 0)));
    }

    #[test]
    fn memory_rejects_layer_without_live_data() {
        let base = SandboxMemoryLayout::BASE_ADDRESS as u64;
        assert!(SnapshotLayer::new(blob(1, base), Box::new([])).is_err());
    }

    #[test]
    fn layer_rejects_adjacent_ranges() {
        let blob = blob(2, SandboxMemoryLayout::BASE_ADDRESS as u64);

        assert!(
            SnapshotLayer::new(blob, Box::new([0..PAGE_SIZE, PAGE_SIZE..2 * PAGE_SIZE]),).is_err()
        );
    }

    #[test]
    fn memory_rejects_overlapping_blob_gpas() {
        let gpa = SandboxMemoryLayout::BASE_ADDRESS as u64;
        let first = SnapshotLayer::new(blob(2, gpa), single_range(0..PAGE_SIZE)).unwrap();
        let second =
            SnapshotLayer::new(blob(1, gpa + PAGE_SIZE as u64), single_range(0..PAGE_SIZE))
                .unwrap();

        assert!(SnapshotMemory::new(Box::new([first, second]), stub_page_tables()).is_err());
    }

    #[test]
    fn memory_canonicalizes_layers() {
        let base = SandboxMemoryLayout::BASE_ADDRESS as u64;
        let lower = SnapshotLayer::new(blob(1, base), single_range(0..PAGE_SIZE)).unwrap();
        let upper =
            SnapshotLayer::new(blob(1, base + PAGE_SIZE as u64), single_range(0..PAGE_SIZE))
                .unwrap();

        let memory = SnapshotMemory::new(Box::new([upper, lower]), stub_page_tables()).unwrap();

        assert_eq!(memory.layers()[0].blob().data_gpa_range().gpa_start(), base);
    }

    #[test]
    fn resolve_accepts_live_edges_and_rejects_holes() {
        let gpa = SandboxMemoryLayout::BASE_ADDRESS as u64;
        let layer = SnapshotLayer::new(
            blob(3, gpa),
            Box::new([0..PAGE_SIZE, 2 * PAGE_SIZE..3 * PAGE_SIZE]),
        )
        .unwrap();
        let memory = SnapshotMemory::new(Box::new([layer]), stub_page_tables()).unwrap();

        assert_eq!(memory.resolve(gpa, 1), Some((0, 0)));
        assert_eq!(
            memory.resolve(gpa + (3 * PAGE_SIZE - 1) as u64, 1),
            Some((0, 3 * PAGE_SIZE - 1))
        );
        assert_eq!(memory.resolve(gpa + PAGE_SIZE as u64, 1), None);
        assert_eq!(memory.resolve(gpa + (PAGE_SIZE - 1) as u64, 2), None);
    }

    #[test]
    fn sparse_reads_reject_holes_and_addresses_past_the_blob() {
        let gpa = SandboxMemoryLayout::BASE_ADDRESS as u64;
        let layer = SnapshotLayer::new(
            blob(3, gpa),
            Box::new([0..PAGE_SIZE, 2 * PAGE_SIZE..3 * PAGE_SIZE]),
        )
        .unwrap();
        let memory = SnapshotMemory::new(Box::new([layer]), stub_page_tables()).unwrap();
        let (backing, _) = SnapshotMemoryBacking::from_snapshot(Arc::new(memory))
            .unwrap()
            .build();
        let mut byte = [0u8; 1];

        assert!(
            backing
                .read_snapshot_gpa(gpa + PAGE_SIZE as u64 + 1, &mut byte)
                .is_err()
        );
        assert!(
            backing
                .read_snapshot_gpa(gpa + 3 * PAGE_SIZE as u64 + 1, &mut byte)
                .is_err()
        );
    }

    #[test]
    fn memory_rejects_excess_mappings() {
        let gpa = SandboxMemoryLayout::BASE_ADDRESS as u64;
        let data_pages = 2 * MAX_SNAPSHOT_MAPPINGS + 1;
        let live_data = (0..=MAX_SNAPSHOT_MAPPINGS)
            .map(|index| {
                let start = 2 * index * PAGE_SIZE;
                start..start + PAGE_SIZE
            })
            .collect();
        let layer = SnapshotLayer::new(blob(data_pages, gpa), live_data).unwrap();

        assert!(SnapshotMemory::new(Box::new([layer]), stub_page_tables()).is_err());
    }

    #[cfg(not(miri))]
    mod properties {
        use proptest::prelude::*;

        use super::*;

        proptest! {
            #![proptest_config(ProptestConfig::with_cases(32))]

            #[test]
            fn layer_rejects_unaligned_range(offset in 1usize..PAGE_SIZE) {
                let blob = blob(1, SandboxMemoryLayout::BASE_ADDRESS as u64);
                let result = SnapshotLayer::new(
                    blob,
                    single_range(offset..PAGE_SIZE),
                );
                prop_assert!(result.is_err());
            }

            #[test]
            fn layer_rejects_range_past_blob(extra_pages in 1usize..32) {
                let blob = blob(1, SandboxMemoryLayout::BASE_ADDRESS as u64);
                let result = SnapshotLayer::new(
                    blob,
                    single_range(0..(1 + extra_pages) * PAGE_SIZE),
                );
                prop_assert!(result.is_err());
            }
        }
    }
}
