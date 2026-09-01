// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 The Hyperlight Authors.

use std::borrow::Cow;
use std::ops::Range;
use std::sync::{Arc, OnceLock};

use hyperlight_common::vmem::PAGE_SIZE;
use sha2::{Digest as _, Sha256};

use crate::Result;
use crate::mem::layout::SandboxMemoryLayout;
use crate::mem::shared_mem::{ReadonlySharedMemory, SharedMemory};

// KVM guarantees at least 32 memory slots. Scratch occupies one and public
// dynamic mappings retain one, leaving 30 for snapshot extents.
pub(crate) const MAX_SNAPSHOT_LIVE_EXTENTS: usize = 30;
pub(crate) const MAX_SNAPSHOT_BLOBS: usize = MAX_SNAPSHOT_LIVE_EXTENTS + 1;
pub(crate) const MAX_SNAPSHOT_RETAINED_BYTES: usize =
    SandboxMemoryLayout::MAX_MEMORY_SIZE.saturating_mul(2);

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

/// Immutable layer backing. `data` describes the guest-addressable prefix.
/// `page_tables` describes the host-only tail copied into scratch on restore.
#[derive(Debug)]
pub(crate) struct SnapshotBlob {
    storage: ReadonlySharedMemory,
    data: Option<SnapshotDataRange>,
    page_tables: Option<Range<usize>>,
    sha256: OnceLock<[u8; 32]>,
}

impl SnapshotBlob {
    pub(crate) fn new(
        storage: ReadonlySharedMemory,
        data_gpa_start: Option<u64>,
        page_tables: Option<Range<usize>>,
        scratch_base_gpa: u64,
    ) -> Result<Self> {
        let storage_len = storage.mem_size();
        let data_len = storage.guest_mapped_size();
        let data = validate_snapshot_blob_layout(
            storage_len,
            data_gpa_start,
            data_len,
            page_tables.as_ref(),
            scratch_base_gpa,
            page_size::get(),
        )?;

        Ok(Self {
            storage,
            data,
            page_tables,
            sha256: OnceLock::new(),
        })
    }

    pub(crate) fn storage(&self) -> &ReadonlySharedMemory {
        &self.storage
    }

    pub(crate) fn data(&self) -> Option<&SnapshotDataRange> {
        self.data.as_ref()
    }

    pub(crate) fn page_tables(&self) -> Option<&Range<usize>> {
        self.page_tables.as_ref()
    }

    pub(crate) fn sha256(&self) -> [u8; 32] {
        *self
            .sha256
            .get_or_init(|| Sha256::digest(self.storage.as_slice()).into())
    }

    fn storage_len(&self) -> usize {
        self.storage.mem_size()
    }
}

#[derive(Clone, Debug)]
pub(crate) struct SnapshotLayer {
    blob: Arc<SnapshotBlob>,
    live_data: Box<[Range<usize>]>,
}

impl SnapshotLayer {
    pub(crate) fn new(blob: Arc<SnapshotBlob>, live_data: Box<[Range<usize>]>) -> Result<Self> {
        let data_len = blob.data.as_ref().map(SnapshotDataRange::len);
        validate_snapshot_live_data(data_len, live_data.iter().cloned(), page_size::get())?;

        Ok(Self { blob, live_data })
    }

    pub(crate) fn blob(&self) -> &Arc<SnapshotBlob> {
        &self.blob
    }

    pub(crate) fn live_data(&self) -> &[Range<usize>] {
        &self.live_data
    }

    pub(crate) fn resolve(&self, gpa: u64, len: usize) -> Option<usize> {
        let data = self.blob.data()?;
        let relative = gpa.checked_sub(data.gpa_start())?;
        let offset = usize::try_from(relative).ok()?;
        let end = offset.checked_add(len)?;
        if end > data.len() {
            return None;
        }
        self.live_data
            .iter()
            .any(|range| range.start <= offset && end <= range.end)
            .then_some(offset)
    }

    fn resolve_live_chunk(&self, gpa: u64) -> Option<(usize, usize)> {
        let data = self.blob.data()?;
        let offset = usize::try_from(gpa.checked_sub(data.gpa_start())?).ok()?;
        let range_index = self
            .live_data
            .partition_point(|range| range.start <= offset)
            .checked_sub(1)?;
        let range = &self.live_data[range_index];
        (offset < range.end).then_some((offset, range.end - offset))
    }
}

#[derive(Debug)]
pub(crate) struct SnapshotMemory {
    layers: Box<[SnapshotLayer]>,
    active_page_table_layer: usize,
}

impl SnapshotMemory {
    pub(crate) fn new(
        mut layers: Box<[SnapshotLayer]>,
        active_page_table_layer: usize,
    ) -> Result<Self> {
        validate_snapshot_layer_count(layers.len())?;
        let Some(active_layer) = layers.get(active_page_table_layer) else {
            return Err(crate::new_error!(
                "active snapshot page-table layer {} is out of bounds",
                active_page_table_layer
            ));
        };
        if active_layer.blob.page_tables.is_none() {
            return Err(crate::new_error!(
                "active snapshot layer has no page tables"
            ));
        }
        let active_blob = active_layer.blob.clone();
        layers.sort_by_key(|layer| {
            layer
                .blob
                .data()
                .map_or(u64::MAX, SnapshotDataRange::gpa_start)
        });
        let active_page_table_layer = layers
            .iter()
            .position(|layer| Arc::ptr_eq(&layer.blob, &active_blob))
            .ok_or_else(|| crate::new_error!("active snapshot layer is missing"))?;

        let mut extent_count = 0usize;
        let mut mapped_bytes = 0usize;
        let mut retained_bytes = 0usize;
        for (index, layer) in layers.iter().enumerate() {
            if layers[..index]
                .iter()
                .any(|other| Arc::ptr_eq(&other.blob, &layer.blob))
            {
                return Err(crate::new_error!(
                    "snapshot references the same blob more than once"
                ));
            }

            extent_count = extent_count
                .checked_add(layer.live_data.len())
                .ok_or_else(|| crate::new_error!("snapshot live-extent count overflows"))?;
            for range in &layer.live_data {
                mapped_bytes = mapped_bytes
                    .checked_add(range.end - range.start)
                    .ok_or_else(|| crate::new_error!("snapshot mapped byte count overflows"))?;
            }
            retained_bytes = retained_bytes
                .checked_add(layer.blob.storage_len())
                .ok_or_else(|| crate::new_error!("snapshot retained byte count overflows"))?;
        }
        validate_snapshot_totals(extent_count, mapped_bytes, retained_bytes)?;
        validate_sorted_snapshot_gpa_ranges(
            layers
                .iter()
                .filter_map(|layer| layer.blob.data.as_ref().map(SnapshotDataRange::gpa_range)),
        )?;

        Ok(Self {
            layers,
            active_page_table_layer,
        })
    }

    pub(crate) fn from_flat(
        storage: ReadonlySharedMemory,
        gpa_start: u64,
        data_len: usize,
        page_tables: Range<usize>,
        scratch_base_gpa: u64,
    ) -> Result<Self> {
        let blob = Arc::new(SnapshotBlob::new(
            storage,
            (data_len != 0).then_some(gpa_start),
            Some(page_tables),
            scratch_base_gpa,
        )?);
        let live_data: Box<[Range<usize>]> = if data_len == 0 {
            Box::new([])
        } else {
            single_range(0..data_len)
        };
        let layer = SnapshotLayer::new(blob, live_data)?;
        Self::new(Box::new([layer]), 0)
    }

    #[cfg(test)]
    pub(crate) fn mem_size(&self) -> usize {
        self.layers[self.active_page_table_layer]
            .blob
            .storage()
            .mem_size()
    }

    pub(crate) fn layers(&self) -> &[SnapshotLayer] {
        &self.layers
    }

    pub(crate) fn active_page_table_layer(&self) -> usize {
        self.active_page_table_layer
    }

    pub(crate) fn gpa_span_len(&self) -> usize {
        let end = self
            .layers
            .iter()
            .filter_map(|layer| layer.blob.data.as_ref())
            .map(|data| data.0.end)
            .max()
            .unwrap_or(SandboxMemoryLayout::BASE_ADDRESS as u64);
        usize::try_from(end - SandboxMemoryLayout::BASE_ADDRESS as u64)
            .expect("SnapshotMemory validates its GPA span")
    }

    pub(crate) fn page_table_len(&self) -> usize {
        self.layers[self.active_page_table_layer]
            .blob
            .page_tables
            .as_ref()
            .expect("SnapshotMemory requires active page tables")
            .len()
    }

    pub(crate) fn resolve(&self, gpa: u64, len: usize) -> Option<(usize, usize)> {
        let index = self.layers.partition_point(|layer| {
            layer
                .blob()
                .data()
                .is_some_and(|data| data.gpa_start() <= gpa)
        });
        let index = index.checked_sub(1)?;
        self.layers[index]
            .resolve(gpa, len)
            .map(|offset| (index, offset))
    }

    pub(crate) fn resolve_live_chunk(&self, gpa: u64) -> Option<(usize, usize, usize)> {
        let index = self.layers.partition_point(|layer| {
            layer
                .blob()
                .data()
                .is_some_and(|data| data.gpa_start() <= gpa)
        });
        let index = index.checked_sub(1)?;
        self.layers[index]
            .resolve_live_chunk(gpa)
            .map(|(offset, len)| (index, offset, len))
    }

    #[cfg(test)]
    pub(crate) fn read_gpa(&self, gpa: u64, destination: &mut [u8]) -> Result<()> {
        let (layer_index, offset) = self
            .resolve(gpa, destination.len())
            .ok_or_else(|| crate::new_error!("snapshot GPA range is not live: {gpa:#x}"))?;
        let source = self.layers[layer_index]
            .blob
            .storage()
            .as_slice()
            .get(offset..offset + destination.len())
            .ok_or_else(|| crate::new_error!("snapshot GPA range is out of bounds"))?;
        destination.copy_from_slice(source);
        Ok(())
    }

    pub(crate) fn read_page_tables(
        &self,
        pt_gpa_base: u64,
        gpa: u64,
        destination: &mut [u8],
    ) -> Result<()> {
        let layer = &self.layers[self.active_page_table_layer];
        let page_tables = layer
            .blob
            .page_tables()
            .ok_or_else(|| crate::new_error!("active snapshot layer has no page tables"))?;
        let offset = usize::try_from(
            gpa.checked_sub(pt_gpa_base)
                .ok_or_else(|| crate::new_error!("page-table GPA is below its base"))?,
        )?;
        let end = offset
            .checked_add(destination.len())
            .ok_or_else(|| crate::new_error!("page-table read range overflows"))?;
        if end > page_tables.end - page_tables.start {
            return Err(crate::new_error!("page-table read range is out of bounds"));
        }
        destination.copy_from_slice(
            &layer.blob.storage().as_slice()[page_tables.start + offset..page_tables.start + end],
        );
        Ok(())
    }

    pub(crate) fn flat_image(&self) -> Result<Cow<'_, [u8]>> {
        let data_len = self.gpa_span_len();
        let active = &self.layers[self.active_page_table_layer];
        let page_tables = active
            .blob
            .page_tables()
            .ok_or_else(|| crate::new_error!("active snapshot layer has no page tables"))?;
        if self.layers.len() == 1
            && active.blob.data().is_some_and(|data| {
                data.gpa_start() == SandboxMemoryLayout::BASE_ADDRESS as u64
                    && data.len() == data_len
                    && active.live_data.len() == 1
                    && active.live_data[0] == (0..data_len)
            })
        {
            return Ok(Cow::Borrowed(active.blob.storage().as_slice()));
        }

        let image_len = self.flat_image_len()?;
        let mut image = vec![0u8; image_len];
        for layer in &self.layers {
            let Some(data) = layer.blob.data() else {
                continue;
            };
            let target_base = usize::try_from(
                data.gpa_start()
                    .checked_sub(SandboxMemoryLayout::BASE_ADDRESS as u64)
                    .ok_or_else(|| crate::new_error!("snapshot blob starts below base"))?,
            )?;
            for range in &layer.live_data {
                let target_start = target_base
                    .checked_add(range.start)
                    .ok_or_else(|| crate::new_error!("flat snapshot offset overflows"))?;
                let target_end = target_start
                    .checked_add(range.len())
                    .ok_or_else(|| crate::new_error!("flat snapshot range overflows"))?;
                let destination = image
                    .get_mut(target_start..target_end)
                    .ok_or_else(|| crate::new_error!("flat snapshot range is out of bounds"))?;
                let source = layer
                    .blob
                    .storage()
                    .as_slice()
                    .get(range.clone())
                    .ok_or_else(|| crate::new_error!("snapshot source range is out of bounds"))?;
                destination.copy_from_slice(source);
            }
        }
        let page_table_bytes = active
            .blob
            .storage()
            .as_slice()
            .get(page_tables.clone())
            .ok_or_else(|| crate::new_error!("snapshot page-table range is out of bounds"))?;
        let page_table_end = data_len
            .checked_add(page_table_bytes.len())
            .ok_or_else(|| crate::new_error!("flat snapshot page-table range overflows"))?;
        image[data_len..page_table_end].copy_from_slice(page_table_bytes);
        Ok(Cow::Owned(image))
    }

    pub(crate) fn flat_image_len(&self) -> Result<usize> {
        let data_len = self.gpa_span_len();
        let page_tables = self.layers[self.active_page_table_layer]
            .blob
            .page_tables()
            .ok_or_else(|| crate::new_error!("active snapshot layer has no page tables"))?;
        let logical_len = data_len
            .checked_add(page_tables.len())
            .ok_or_else(|| crate::new_error!("flat snapshot image size overflows"))?;
        logical_len
            .checked_next_multiple_of(page_size::get())
            .ok_or_else(|| crate::new_error!("flat snapshot image padding overflows"))
    }
}

pub(crate) fn validate_snapshot_blob_layout(
    storage_len: usize,
    data_gpa_start: Option<u64>,
    data_len: usize,
    page_tables: Option<&Range<usize>>,
    scratch_base_gpa: u64,
    host_page_size: usize,
) -> Result<Option<SnapshotDataRange>> {
    if data_gpa_start.is_some() && data_len == 0 {
        return Err(crate::new_error!("snapshot layer data size is zero"));
    }
    if storage_len == 0
        || !storage_len.is_multiple_of(host_page_size)
        || !is_page_aligned(data_len, host_page_size)
        || data_len > storage_len
    {
        return Err(crate::new_error!(
            "snapshot layer storage or data size is invalid"
        ));
    }

    let data = match (data_gpa_start, data_len) {
        (Some(gpa_start), data_len) if data_len != 0 => {
            if !is_page_aligned_u64(gpa_start, host_page_size) {
                return Err(crate::new_error!("snapshot layer GPA is not aligned"));
            }
            if gpa_start < SandboxMemoryLayout::BASE_ADDRESS as u64 {
                return Err(crate::new_error!(
                    "snapshot layer starts below BASE_ADDRESS"
                ));
            }
            let gpa_end = gpa_start
                .checked_add(u64::try_from(data_len)?)
                .ok_or_else(|| crate::new_error!("snapshot layer GPA range overflows"))?;
            if gpa_end > scratch_base_gpa {
                return Err(crate::new_error!(
                    "snapshot layer data overlaps scratch memory"
                ));
            }
            Some(SnapshotDataRange(gpa_start..gpa_end))
        }
        (None, 0) => None,
        _ => return Err(crate::new_error!("snapshot blob data GPA is missing")),
    };

    let page_tables_valid = match page_tables {
        Some(range) => {
            range.start == data_len
                && range.start < range.end
                && range.end <= storage_len
                && storage_len
                    .checked_sub(range.end)
                    .is_some_and(|padding| padding < host_page_size)
                && is_page_aligned(range.start, host_page_size)
                && range.end.is_multiple_of(PAGE_SIZE)
        }
        None => data_len == storage_len,
    };
    if !page_tables_valid {
        return Err(crate::new_error!(
            "snapshot layer page-table range is invalid"
        ));
    }

    Ok(data)
}

pub(crate) fn validate_snapshot_live_data(
    data_len: Option<usize>,
    live_data: impl IntoIterator<Item = Range<usize>>,
    host_page_size: usize,
) -> Result<()> {
    let mut previous_end = None;
    for range in live_data {
        if data_len.is_none() {
            return Err(crate::new_error!(
                "page-table-only snapshot blob has live data"
            ));
        }
        if range.start >= range.end
            || !is_page_aligned(range.start, host_page_size)
            || !is_page_aligned(range.end, host_page_size)
            || range.end > data_len.unwrap_or(0)
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
    Ok(())
}

pub(crate) fn validate_snapshot_layer_count(layer_count: usize) -> Result<()> {
    if layer_count == 0 || layer_count > MAX_SNAPSHOT_BLOBS {
        return Err(crate::new_error!(
            "snapshot layer count {} is invalid",
            layer_count
        ));
    }
    Ok(())
}

pub(crate) fn validate_snapshot_totals(
    extent_count: usize,
    mapped_bytes: usize,
    retained_bytes: usize,
) -> Result<()> {
    if extent_count > MAX_SNAPSHOT_LIVE_EXTENTS {
        return Err(crate::new_error!(
            "snapshot live-extent count {} exceeds {}",
            extent_count,
            MAX_SNAPSHOT_LIVE_EXTENTS
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

    fn blob(data_pages: usize, page_table_pages: usize, gpa_start: u64) -> Arc<SnapshotBlob> {
        let data_len = data_pages * PAGE_SIZE;
        let storage_len = (data_pages + page_table_pages) * PAGE_SIZE;
        let storage = ExclusiveSharedMemory::new(storage_len)
            .unwrap()
            .freeze(data_len)
            .unwrap();
        Arc::new(
            SnapshotBlob::new(
                storage,
                (data_len != 0).then_some(gpa_start),
                (page_table_pages != 0).then_some(data_len..storage_len),
                SCRATCH_BASE_GPA,
            )
            .unwrap(),
        )
    }

    #[test]
    fn flat_snapshot_memory_is_valid() {
        let data_len = 2 * PAGE_SIZE;
        let storage_len = 3 * PAGE_SIZE;
        let storage = ExclusiveSharedMemory::new(storage_len)
            .unwrap()
            .freeze(data_len)
            .unwrap();

        let memory = SnapshotMemory::from_flat(
            storage,
            SandboxMemoryLayout::BASE_ADDRESS as u64,
            data_len,
            data_len..storage_len,
            SCRATCH_BASE_GPA,
        )
        .unwrap();

        assert_eq!(memory.mem_size(), storage_len);
    }

    #[test]
    fn blob_rejects_guest_mapped_page_tables() {
        let storage = ExclusiveSharedMemory::new(2 * PAGE_SIZE)
            .unwrap()
            .freeze(2 * PAGE_SIZE)
            .unwrap();

        assert!(
            SnapshotBlob::new(
                storage,
                Some(SandboxMemoryLayout::BASE_ADDRESS as u64),
                Some(PAGE_SIZE..2 * PAGE_SIZE),
                SCRATCH_BASE_GPA,
            )
            .is_err()
        );
    }

    #[test]
    fn blob_rejects_overflowing_gpa_range() {
        let storage = ExclusiveSharedMemory::new(PAGE_SIZE)
            .unwrap()
            .freeze(PAGE_SIZE)
            .unwrap();
        let gpa_start = u64::MAX - PAGE_SIZE as u64 + 1;

        assert!(SnapshotBlob::new(storage, Some(gpa_start), None, u64::MAX,).is_err());
    }

    #[test]
    fn blob_rejects_data_without_gpa() {
        let storage = ExclusiveSharedMemory::new(PAGE_SIZE)
            .unwrap()
            .freeze(PAGE_SIZE)
            .unwrap();

        assert!(SnapshotBlob::new(storage, None, None, SCRATCH_BASE_GPA).is_err());
    }

    #[test]
    fn page_table_only_active_layer_is_valid() {
        let storage = ExclusiveSharedMemory::new(PAGE_SIZE)
            .unwrap()
            .freeze(0)
            .unwrap();
        let blob = Arc::new(
            SnapshotBlob::new(storage, None, Some(0..PAGE_SIZE), SCRATCH_BASE_GPA).unwrap(),
        );
        let layer = SnapshotLayer::new(blob, Box::new([])).unwrap();

        assert!(SnapshotMemory::new(Box::new([layer]), 0).is_ok());
    }

    #[test]
    fn reads_page_tables_from_data_free_active_layer() {
        let base = SandboxMemoryLayout::BASE_ADDRESS as u64;
        let ancestor_storage = ReadonlySharedMemory::from_bytes(
            &[vec![0x11; PAGE_SIZE], vec![0x22; PAGE_SIZE]].concat(),
            PAGE_SIZE,
        )
        .unwrap();
        let ancestor_blob = Arc::new(
            SnapshotBlob::new(
                ancestor_storage,
                Some(base),
                Some(PAGE_SIZE..2 * PAGE_SIZE),
                SCRATCH_BASE_GPA,
            )
            .unwrap(),
        );
        let ancestor = SnapshotLayer::new(ancestor_blob, single_range(0..PAGE_SIZE)).unwrap();
        let mut active_storage = ExclusiveSharedMemory::new(PAGE_SIZE).unwrap();
        active_storage
            .copy_from_slice(&vec![0x33; PAGE_SIZE], 0)
            .unwrap();
        let active_storage = active_storage.freeze(0).unwrap();
        let active_blob = Arc::new(
            SnapshotBlob::new(active_storage, None, Some(0..PAGE_SIZE), SCRATCH_BASE_GPA).unwrap(),
        );
        let active = SnapshotLayer::new(active_blob, Box::new([])).unwrap();
        let memory = SnapshotMemory::new(Box::new([ancestor, active]), 1).unwrap();
        let mut byte = [0u8; 1];

        memory.read_page_tables(0x8000, 0x8fff, &mut byte).unwrap();

        assert_eq!(byte, [0x33]);
        assert_eq!(memory.resolve(base, PAGE_SIZE), Some((0, 0)));
    }

    #[test]
    fn active_layer_requires_page_tables() {
        let blob = blob(1, 0, SandboxMemoryLayout::BASE_ADDRESS as u64);
        let layer = SnapshotLayer::new(blob, single_range(0..PAGE_SIZE)).unwrap();

        assert!(SnapshotMemory::new(Box::new([layer]), 0).is_err());
    }

    #[test]
    fn layer_rejects_adjacent_extents() {
        let blob = blob(2, 1, SandboxMemoryLayout::BASE_ADDRESS as u64);

        assert!(
            SnapshotLayer::new(blob, Box::new([0..PAGE_SIZE, PAGE_SIZE..2 * PAGE_SIZE]),).is_err()
        );
    }

    #[test]
    fn memory_rejects_overlapping_blob_gpas() {
        let gpa = SandboxMemoryLayout::BASE_ADDRESS as u64;
        let first = SnapshotLayer::new(blob(2, 1, gpa), single_range(0..PAGE_SIZE)).unwrap();
        let second = SnapshotLayer::new(
            blob(1, 1, gpa + PAGE_SIZE as u64),
            single_range(0..PAGE_SIZE),
        )
        .unwrap();

        assert!(SnapshotMemory::new(Box::new([first, second]), 1).is_err());
    }

    #[test]
    fn memory_canonicalizes_layers_and_active_index() {
        let base = SandboxMemoryLayout::BASE_ADDRESS as u64;
        let lower = SnapshotLayer::new(blob(1, 1, base), single_range(0..PAGE_SIZE)).unwrap();
        let upper = SnapshotLayer::new(
            blob(1, 1, base + PAGE_SIZE as u64),
            single_range(0..PAGE_SIZE),
        )
        .unwrap();
        let active_blob = upper.blob().clone();

        let memory = SnapshotMemory::new(Box::new([upper, lower]), 0).unwrap();

        assert_eq!(memory.layers()[0].blob().data().unwrap().gpa_start(), base);
        assert_eq!(memory.active_page_table_layer(), 1);
        assert!(Arc::ptr_eq(
            memory.layers()[memory.active_page_table_layer()].blob(),
            &active_blob,
        ));
    }

    #[test]
    fn resolve_accepts_live_edges_and_rejects_holes() {
        let gpa = SandboxMemoryLayout::BASE_ADDRESS as u64;
        let layer = SnapshotLayer::new(
            blob(3, 1, gpa),
            Box::new([0..PAGE_SIZE, 2 * PAGE_SIZE..3 * PAGE_SIZE]),
        )
        .unwrap();
        let memory = SnapshotMemory::new(Box::new([layer]), 0).unwrap();

        assert_eq!(memory.resolve(gpa, 1), Some((0, 0)));
        assert_eq!(
            memory.resolve(gpa + (3 * PAGE_SIZE - 1) as u64, 1),
            Some((0, 3 * PAGE_SIZE - 1))
        );
        assert_eq!(memory.resolve(gpa + PAGE_SIZE as u64, 1), None);
        assert_eq!(memory.resolve(gpa + (PAGE_SIZE - 1) as u64, 2), None);
    }

    #[test]
    fn memory_rejects_excess_live_extents() {
        let gpa = SandboxMemoryLayout::BASE_ADDRESS as u64;
        let data_pages = 2 * MAX_SNAPSHOT_LIVE_EXTENTS + 1;
        let live_data = (0..=MAX_SNAPSHOT_LIVE_EXTENTS)
            .map(|index| {
                let start = 2 * index * PAGE_SIZE;
                start..start + PAGE_SIZE
            })
            .collect();
        let layer = SnapshotLayer::new(blob(data_pages, 1, gpa), live_data).unwrap();

        assert!(SnapshotMemory::new(Box::new([layer]), 0).is_err());
    }

    #[cfg(not(miri))]
    mod properties {
        use proptest::prelude::*;

        use super::*;

        proptest! {
            #![proptest_config(ProptestConfig::with_cases(32))]

            #[test]
            fn layer_rejects_unaligned_extent(offset in 1usize..PAGE_SIZE) {
                let blob = blob(1, 1, SandboxMemoryLayout::BASE_ADDRESS as u64);
                let result = SnapshotLayer::new(
                    blob,
                    single_range(offset..PAGE_SIZE),
                );
                prop_assert!(result.is_err());
            }

            #[test]
            fn layer_rejects_extent_past_blob(extra_pages in 1usize..32) {
                let blob = blob(1, 1, SandboxMemoryLayout::BASE_ADDRESS as u64);
                let result = SnapshotLayer::new(
                    blob,
                    single_range(0..(1 + extra_pages) * PAGE_SIZE),
                );
                prop_assert!(result.is_err());
            }
        }
    }
}
