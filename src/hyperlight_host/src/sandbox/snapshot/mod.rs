// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 The Hyperlight Authors.

mod file;
mod file_tests;
mod memory;
mod tripwires;

use std::cell::Cell;
use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;

pub(crate) use file::host_cpu_vendor_golden_tag;
pub use file::reference::{OciDigest, OciReference, OciTag};
use hyperlight_common::flatbuffer_wrappers::host_function_details::HostFunctionDetails;
use hyperlight_common::layout::{io_page, scratch_base_gpa, scratch_base_gva};
use hyperlight_common::vmem;
use hyperlight_common::vmem::{
    BasicMapping, CowMapping, Mapping, MappingKind, PAGE_SIZE, SpaceAwareMapping, SpaceId, TableOps,
};
use tracing::{Span, instrument};

#[cfg(test)]
pub(crate) use self::memory::SnapshotLayer;
pub(crate) use self::memory::{SnapshotBlob, SnapshotMemory, SnapshotMemoryBacking};
use crate::Result;
use crate::hypervisor::regs::CommonSpecialRegisters;
#[cfg(target_arch = "x86_64")]
use crate::hypervisor::regs::MsrEntry;
use crate::mem::exe::{ExeInfo, LoadInfo};
use crate::mem::layout::SandboxMemoryLayout;
use crate::mem::memory_region::{GuestMemoryRegion, MemoryRegion, MemoryRegionFlags};
use crate::mem::mgr::{GuestPageTableBuffer, GuestPhysicalMemoryView};
use crate::mem::shared_mem::{HostSharedMemory, ReadonlySharedMemory};
use crate::sandbox::SandboxConfiguration;
use crate::sandbox::uninitialized::{GuestBinary, GuestEnvironment};

const PTE_SIZE: usize = size_of::<vmem::PageTableEntry>();
// This per-capture budget is shared by every root to bound work from malformed
// or excessive root lists. Shared tables are read once across all roots.
//
// The walk covers the whole virtual address space while physical memory stops
// at MAX_MEMORY_SIZE, so a guest can alias one frame across far more virtual
// addresses than it has pages. Sizing the budget to the page count bounds the
// walk by the most content a snapshot can hold: aliased mappings past that
// point resolve to pages already captured. The factor of two covers reads of
// the levels above the leaves.
//
// Two address spaces that each map all of memory through wholly unshared
// tables exceed this and are refused. They describe the same pages twice.
const MAX_SNAPSHOT_PAGE_TABLE_READS: usize = 2 * (SandboxMemoryLayout::MAX_MEMORY_SIZE / PAGE_SIZE);

pub(crate) struct PageTableReader<'a> {
    memory: &'a GuestPhysicalMemoryView<'a>,
    root: u64,
    reads: Cell<usize>,
    failure: Cell<Option<&'static str>>,
}
impl<'a> PageTableReader<'a> {
    pub(crate) fn new(memory: &'a GuestPhysicalMemoryView<'a>, root: u64) -> Self {
        Self {
            memory,
            root,
            reads: Cell::new(0),
            failure: Cell::new(None),
        }
    }

    /// Report whatever the walk suppressed. A walk that reads past the budget
    /// or off a backing keeps going against not-present entries, so callers
    /// that need whole coverage must ask before trusting the result.
    pub(crate) fn finish(&self) -> Result<()> {
        match self.failure.get() {
            Some(failure) => Err(crate::new_error!("{}", failure)),
            None => Ok(()),
        }
    }
}
impl<'a> hyperlight_common::vmem::TableReadOps for PageTableReader<'a> {
    type TableAddr = u64;
    fn entry_addr(addr: u64, offset: u64) -> u64 {
        addr.saturating_add(offset)
    }
    unsafe fn read_entry(&self, addr: u64) -> vmem::PageTableEntry {
        if self.failure.get().is_some() {
            return 0;
        }
        let reads = self.reads.get();
        if reads >= MAX_SNAPSHOT_PAGE_TABLE_READS {
            self.failure
                .set(Some("snapshot page-table walk limit exceeded"));
            return 0;
        }
        self.reads.set(reads + 1);

        let mut pte_bytes = [0u8; PTE_SIZE];
        if self.memory.read(addr, &mut pte_bytes).is_err() {
            self.failure
                .set(Some("snapshot page-table walk accessed unbacked memory"));
            return 0;
        }
        vmem::PageTableEntry::from_le_bytes(pte_bytes)
    }
    #[allow(clippy::unnecessary_cast)]
    fn to_phys(addr: u64) -> vmem::PhysAddr {
        addr as vmem::PhysAddr
    }
    #[allow(clippy::unnecessary_cast)]
    fn from_phys(addr: vmem::PhysAddr) -> u64 {
        addr as u64
    }
    fn root_table(&self) -> u64 {
        self.root
    }
}
impl<'a> core::convert::AsRef<PageTableReader<'a>> for PageTableReader<'a> {
    fn as_ref(&self) -> &Self {
        self
    }
}

/// Presently, a snapshot can be of a preinitialised sandbox, which
/// still needs an initialise function called in order to determine
/// how to call into it, or of an already-properly-initialised sandbox
/// which can be immediately called into. This keeps track of the
/// difference.
///
/// TODO: this should not necessarily be around in the long term:
/// ideally we would just preinitialise earlier in the snapshot
/// creation process and never need this.
#[derive(Copy, Clone, PartialEq, Eq)]
pub enum NextAction {
    /// A sandbox in the preinitialise state still needs to be
    /// initialised by calling the initialise function
    Initialise(u64),
    /// A sandbox in the ready state can immediately be called into,
    /// using the dispatch function pointer.
    Call(u64),
    /// Only when compiling for tests: a sandbox that cannot actually
    /// be used
    #[cfg(test)]
    None,
}

/// A wrapper around a `SharedMemory` reference and a snapshot
/// of the memory therein
pub struct Snapshot {
    /// Layout object for the sandbox. TODO: get rid of this and
    /// replace with something saner and set up from the guest (early
    /// on?).
    layout: crate::mem::layout::SandboxMemoryLayout,
    /// Memory of the sandbox at the time this snapshot was taken
    memory: Arc<SnapshotMemory>,
    /// Extra debug information about the binary in this snapshot,
    /// from when the binary was first loaded into the snapshot.
    ///
    /// This information is provided on a best-effort basis, and there
    /// is a pretty good chance that it does not exist; generally speaking,
    /// things like persisting a snapshot and reloading it are likely
    /// to destroy this information.
    load_info: LoadInfo,
    /// The address of the top of the guest stack
    stack_top_gva: u64,

    /// Special register state captured from the vCPU during snapshot.
    /// None for snapshots created directly from a binary (before
    /// guest runs).  Some for snapshots taken from a running sandbox.
    /// Note: CR3 in this struct is NOT used on restore, since page
    /// tables are relocated during snapshot.
    sregs: Option<CommonSpecialRegisters>,

    /// The MSRs saved in this snapshot. None before the guest has run.
    #[cfg(target_arch = "x86_64")]
    msrs: Option<Vec<MsrEntry>>,

    /// The next action that should be performed on this snapshot
    next_action: NextAction,

    /// Guest virtual address of the guest binary's ELF entry point
    /// (`load_addr + e_entry - base_va`). Unlike `next_action`, which
    /// transitions to `Call(dispatch_addr)` once the guest has run,
    /// this preserves the original entry across that transition. Used
    /// to fill `AT_ENTRY` in guest core dumps so a debugger can
    /// compute the PIE load bias. 0 if unknown (e.g. an older
    /// on-disk snapshot that predates this field).
    original_entrypoint: u64,

    /// The generation number assigned to this snapshot when it was
    /// taken — i.e. "this is the Nth snapshot taken from the sandbox's
    /// execution path from init to here". Propagated into the
    /// restored sandbox's guest-visible counter so the guest can tell
    /// which snapshot it is currently a clone of.
    snapshot_generation: u64,

    /// Names and signatures of host functions registered on the
    /// sandbox at the time this snapshot was taken. Used by
    /// [`crate::MultiUseSandbox::from_snapshot`] to reject a
    /// `HostFunctions` set that is missing required functions or
    /// has mismatched signatures.
    host_functions: HostFunctionDetails,
}
impl core::convert::AsRef<Snapshot> for Snapshot {
    fn as_ref(&self) -> &Self {
        self
    }
}
impl hyperlight_common::vmem::TableReadOps for Snapshot {
    type TableAddr = u64;
    fn entry_addr(addr: u64, offset: u64) -> u64 {
        addr.saturating_add(offset)
    }
    unsafe fn read_entry(&self, addr: u64) -> vmem::PageTableEntry {
        let mut pte_bytes = [0u8; PTE_SIZE];
        if self
            .memory
            .read_page_tables(self.layout.get_pt_base_gpa(), addr, &mut pte_bytes)
            .is_err()
        {
            // Attacker-controlled data pointed out-of-bounds. We'll
            // default to returning 0 in this case, which, for most
            // architectures (including x86-64 and arm64, the ones we
            // care about presently) will be a not-present entry.
            return 0;
        }
        vmem::PageTableEntry::from_le_bytes(pte_bytes)
    }
    #[allow(clippy::unnecessary_cast)]
    fn to_phys(addr: u64) -> vmem::PhysAddr {
        addr as vmem::PhysAddr
    }
    #[allow(clippy::unnecessary_cast)]
    fn from_phys(addr: vmem::PhysAddr) -> u64 {
        addr as u64
    }
    fn root_table(&self) -> u64 {
        self.root_pt_gpa()
    }
}

/// Return true if `virt_base` is a VA we must not preserve into the
/// rebuilt snapshot page tables: it is either part of the scratch
/// region (re-mapped freshly by `map_specials`) or, on amd64, part of
/// the self-map of the snapshot's own page tables.
fn skip_virt(virt_base: u64, scratch_gva: u64) -> bool {
    if virt_base >= scratch_gva {
        return true;
    }
    if virt_base >= hyperlight_common::layout::SNAPSHOT_PT_GVA_MIN as u64
        && virt_base <= hyperlight_common::layout::SNAPSHOT_PT_GVA_MAX as u64
    {
        return true;
    }
    false
}

fn for_each_mapping_page(
    mapping: &Mapping,
    mut visitor: impl FnMut(u64, u64, MappingKind) -> Result<()>,
) -> Result<()> {
    if !validate_mapping(mapping)? {
        return Ok(());
    }
    let mut offset = 0u64;
    while offset < mapping.len {
        let phys = mapping
            .phys_base
            .checked_add(offset)
            .ok_or_else(|| crate::new_error!("guest physical mapping overflows"))?;
        let virt = mapping
            .virt_base
            .checked_add(offset)
            .ok_or_else(|| crate::new_error!("guest virtual mapping overflows"))?;
        visitor(phys, virt, mapping.kind)?;
        offset = offset
            .checked_add(PAGE_SIZE as u64)
            .ok_or_else(|| crate::new_error!("guest mapping offset overflows"))?;
    }
    Ok(())
}

fn validate_mapping(mapping: &Mapping) -> Result<bool> {
    if mapping.kind == MappingKind::Unmapped {
        return Ok(false);
    }
    if mapping.len == 0
        || !mapping.phys_base.is_multiple_of(PAGE_SIZE as u64)
        || !mapping.virt_base.is_multiple_of(PAGE_SIZE as u64)
        || !mapping.len.is_multiple_of(PAGE_SIZE as u64)
    {
        return Err(crate::new_error!("guest mapping is not page aligned"));
    }
    Ok(true)
}

fn mapping_has_skipped_pages(mapping: &Mapping, scratch_gva: u64) -> Result<bool> {
    let last_page = mapping
        .virt_base
        .checked_add(mapping.len - PAGE_SIZE as u64)
        .ok_or_else(|| crate::new_error!("guest virtual mapping overflows"))?;
    let snapshot_pt_start = hyperlight_common::layout::SNAPSHOT_PT_GVA_MIN as u64;
    let snapshot_pt_end = hyperlight_common::layout::SNAPSHOT_PT_GVA_MAX as u64;
    Ok(last_page >= scratch_gva
        || (mapping.virt_base <= snapshot_pt_end && snapshot_pt_start <= last_page))
}

fn snapshot_mapping_kind(kind: MappingKind) -> Option<MappingKind> {
    match kind {
        MappingKind::Cow(mapping) => Some(MappingKind::Cow(mapping)),
        MappingKind::Basic(mapping) if mapping.writable => Some(MappingKind::Cow(CowMapping {
            readable: mapping.readable,
            executable: mapping.executable,
        })),
        MappingKind::Basic(mapping) => Some(MappingKind::Basic(BasicMapping {
            readable: mapping.readable,
            writable: false,
            executable: mapping.executable,
        })),
        MappingKind::Unmapped => None,
    }
}

fn coalesce_pages(pages: &[bool]) -> Box<[Range<usize>]> {
    let mut ranges: Vec<Range<usize>> = Vec::new();
    for (page_index, reused) in pages.iter().enumerate() {
        if !reused {
            continue;
        }
        let page = page_index * PAGE_SIZE;
        let end = page + PAGE_SIZE;
        if let Some(previous) = ranges.last_mut()
            && previous.end == page
        {
            previous.end = end;
        } else {
            ranges.push(page..end);
        }
    }
    ranges.into_boxed_slice()
}

#[derive(Clone, Copy)]
struct NewPage {
    source_gpa: u64,
    backing: GuestBacking,
}

fn backing_with_offset(backing: GuestBacking, offset: usize) -> Option<GuestBacking> {
    match backing {
        GuestBacking::Snapshot {
            layer_index,
            offset: backing_offset,
        } => Some(GuestBacking::Snapshot {
            layer_index,
            offset: backing_offset.checked_add(offset)?,
        }),
        GuestBacking::Scratch {
            offset: backing_offset,
        } => Some(GuestBacking::Scratch {
            offset: backing_offset.checked_add(offset)?,
        }),
        GuestBacking::Dynamic {
            region_index,
            offset: backing_offset,
        } => Some(GuestBacking::Dynamic {
            region_index,
            offset: backing_offset.checked_add(offset)?,
        }),
    }
}

fn record_backed_pages(
    backing: GuestBacking,
    source_gpa: u64,
    len: usize,
    reused_pages: Option<&mut [Vec<bool>]>,
    new_pages: &mut Vec<NewPage>,
) -> Result<()> {
    if let GuestBacking::Snapshot { offset, .. } = backing
        && (!offset.is_multiple_of(PAGE_SIZE) || !len.is_multiple_of(PAGE_SIZE))
    {
        return Err(crate::new_error!(
            "snapshot backing range is not page aligned"
        ));
    }

    if let GuestBacking::Snapshot {
        layer_index,
        offset,
    } = backing
        && let Some(pages) = reused_pages
    {
        let start = offset / PAGE_SIZE;
        let end = offset
            .checked_add(len)
            .map(|end| end / PAGE_SIZE)
            .ok_or_else(|| crate::new_error!("snapshot backing range overflows"))?;
        pages
            .get_mut(layer_index)
            .and_then(|pages| pages.get_mut(start..end))
            .ok_or_else(|| crate::new_error!("snapshot backing range is out of bounds"))?
            .fill(true);
        return Ok(());
    }

    for offset in (0..len).step_by(PAGE_SIZE) {
        new_pages.push(NewPage {
            source_gpa: source_gpa
                .checked_add(u64::try_from(offset)?)
                .ok_or_else(|| crate::new_error!("snapshot source GPA overflows"))?,
            backing: backing_with_offset(backing, offset)
                .ok_or_else(|| crate::new_error!("snapshot source backing offset overflows"))?,
        });
    }
    Ok(())
}

fn move_partial_host_pages(
    layer_index: usize,
    reused_pages: &mut [bool],
    blob_gpa_start: u64,
    host_page_size: usize,
    new_pages: &mut Vec<NewPage>,
) -> Result<()> {
    if host_page_size < PAGE_SIZE || !host_page_size.is_multiple_of(PAGE_SIZE) {
        return Err(crate::new_error!(
            "host page size {host_page_size} is incompatible with guest page size {PAGE_SIZE}"
        ));
    }
    if host_page_size == PAGE_SIZE {
        return Ok(());
    }

    let pages_per_host_page = host_page_size / PAGE_SIZE;
    for (host_page_index, pages) in reused_pages.chunks_mut(pages_per_host_page).enumerate() {
        if pages.iter().all(|reused| *reused) {
            continue;
        }
        for (page_index, reused) in pages.iter_mut().enumerate() {
            if !*reused {
                continue;
            }
            *reused = false;
            let offset = host_page_index
                .checked_mul(host_page_size)
                .and_then(|offset| offset.checked_add(page_index * PAGE_SIZE))
                .ok_or_else(|| crate::new_error!("snapshot reused-page offset overflows"))?;
            let gpa = blob_gpa_start
                .checked_add(u64::try_from(offset)?)
                .ok_or_else(|| crate::new_error!("snapshot reused-page GPA overflows"))?;
            new_pages.push(NewPage {
                source_gpa: gpa,
                backing: GuestBacking::Snapshot {
                    layer_index,
                    offset,
                },
            });
        }
    }
    Ok(())
}

struct NewPageRun {
    source: Range<u64>,
    destination_start: u64,
}

struct NewPageCopy {
    backing: GuestBacking,
    destination: Range<usize>,
}

fn backing_is_contiguous(first: GuestBacking, next: GuestBacking, len: usize) -> bool {
    match (first, next) {
        (
            GuestBacking::Snapshot {
                layer_index: first_layer,
                offset: first_offset,
            },
            GuestBacking::Snapshot {
                layer_index: next_layer,
                offset: next_offset,
            },
        ) => first_layer == next_layer && first_offset.checked_add(len) == Some(next_offset),
        (
            GuestBacking::Scratch {
                offset: first_offset,
            },
            GuestBacking::Scratch {
                offset: next_offset,
            },
        ) => first_offset.checked_add(len) == Some(next_offset),
        (
            GuestBacking::Dynamic {
                region_index: first_region,
                offset: first_offset,
            },
            GuestBacking::Dynamic {
                region_index: next_region,
                offset: next_offset,
            },
        ) => first_region == next_region && first_offset.checked_add(len) == Some(next_offset),
        _ => false,
    }
}

fn new_page_copies(pages: &[NewPage]) -> Result<Vec<NewPageCopy>> {
    let mut copies = Vec::<NewPageCopy>::new();
    for (index, page) in pages.iter().enumerate() {
        let start = index
            .checked_mul(PAGE_SIZE)
            .ok_or_else(|| crate::new_error!("snapshot page offset overflows"))?;
        let end = start
            .checked_add(PAGE_SIZE)
            .ok_or_else(|| crate::new_error!("snapshot page range overflows"))?;
        if let Some(copy) = copies.last_mut()
            && backing_is_contiguous(copy.backing, page.backing, copy.destination.len())
        {
            copy.destination.end = end;
        } else {
            copies.push(NewPageCopy {
                backing: page.backing,
                destination: start..end,
            });
        }
    }
    Ok(copies)
}

fn new_page_runs(pages: &[NewPage], destination_start: u64) -> Result<Vec<NewPageRun>> {
    let mut runs = Vec::<NewPageRun>::new();
    for (index, page) in pages.iter().enumerate() {
        let source_gpa = page.source_gpa;
        let destination = destination_start
            .checked_add(u64::try_from(index.checked_mul(PAGE_SIZE).ok_or_else(
                || crate::new_error!("snapshot page offset overflows"),
            )?)?)
            .ok_or_else(|| crate::new_error!("snapshot page GPA overflows"))?;
        if let Some(run) = runs.last_mut()
            && run.source.end == source_gpa
        {
            run.source.end = run
                .source
                .end
                .checked_add(PAGE_SIZE as u64)
                .ok_or_else(|| crate::new_error!("snapshot source page run overflows"))?;
        } else {
            let source_end = source_gpa
                .checked_add(PAGE_SIZE as u64)
                .ok_or_else(|| crate::new_error!("snapshot source page overflows"))?;
            runs.push(NewPageRun {
                source: source_gpa..source_end,
                destination_start: destination,
            });
        }
    }
    Ok(runs)
}

fn page_destination(runs: &[NewPageRun], source_gpa: u64) -> Option<u64> {
    let index = runs.partition_point(|run| run.source.end <= source_gpa);
    let run = runs.get(index)?;
    let offset = source_gpa.checked_sub(run.source.start)?;
    (source_gpa < run.source.end).then(|| run.destination_start.checked_add(offset))?
}

fn packed_data_len(page_count: usize, host_page_size: usize) -> Result<usize> {
    let page_bytes = page_count
        .checked_mul(PAGE_SIZE)
        .ok_or_else(|| crate::new_error!("snapshot data size overflows"))?;
    page_bytes
        .checked_next_multiple_of(host_page_size)
        .ok_or_else(|| crate::new_error!("snapshot data padding overflows"))
}

fn first_fit_blob_gpa(data_len: usize, layers: &[SnapshotLayer], scratch_base: u64) -> Option<u64> {
    if data_len == 0 {
        return None;
    }
    let data_len = u64::try_from(data_len).ok()?;
    let mut candidate = SandboxMemoryLayout::BASE_ADDRESS as u64;
    for range in layers
        .iter()
        .filter_map(|layer| layer.blob().data_gpa_range().map(|data| data.gpa_range()))
    {
        let end = candidate.checked_add(data_len)?;
        if end <= range.start {
            return Some(candidate);
        }
        candidate = candidate.max(range.end);
    }
    (candidate.checked_add(data_len)? <= scratch_base).then_some(candidate)
}

fn add_page_table_range(
    tables: &mut BTreeSet<(usize, u8, u64)>,
    root_index: usize,
    start: u64,
    len: u64,
) -> Result<()> {
    if len == 0 {
        return Ok(());
    }
    let end = start
        .checked_add(len - 1)
        .ok_or_else(|| crate::new_error!("snapshot virtual mapping overflows"))?;
    for shift in [39u8, 30, 21] {
        for prefix in (start >> shift)..=(end >> shift) {
            tables.insert((root_index, shift, prefix));
        }
    }
    Ok(())
}

fn add_page_table_page(
    tables: &mut BTreeSet<(usize, u8, u64)>,
    previous_prefixes: &mut [Option<u64>; 3],
    root_index: usize,
    virt_gva: u64,
) {
    for (previous, shift) in previous_prefixes.iter_mut().zip([39u8, 30, 21]) {
        let prefix = virt_gva >> shift;
        if *previous != Some(prefix) {
            tables.insert((root_index, shift, prefix));
            *previous = Some(prefix);
        }
    }
}

fn map_specials(pt_buf: &GuestPageTableBuffer, scratch_size: usize) {
    if let Some((phys_base, virt_base)) = io_page() {
        // Map the IO page
        let mapping = Mapping {
            phys_base,
            virt_base,
            len: PAGE_SIZE as u64,
            kind: MappingKind::Basic(BasicMapping {
                readable: true,
                writable: true,
                executable: false,
            }),
        };
        unsafe { vmem::map(pt_buf, mapping) };
    }
    // Map the scratch region
    let mapping = Mapping {
        phys_base: scratch_base_gpa(scratch_size),
        virt_base: scratch_base_gva(scratch_size),
        len: scratch_size as u64,
        kind: MappingKind::Basic(BasicMapping {
            readable: true,
            writable: true,
            // assume that the guest will map these pages elsewhere if
            // it actually needs to execute from them
            executable: false,
        }),
    };
    unsafe { vmem::map(pt_buf, mapping) };
}

impl Snapshot {
    fn flat_snapshot_memory(
        memory: ReadonlySharedMemory,
        layout: &SandboxMemoryLayout,
        data_len: usize,
        page_table_len: usize,
    ) -> Result<SnapshotMemory> {
        let page_table_end = data_len
            .checked_add(page_table_len)
            .ok_or_else(|| crate::new_error!("snapshot memory size overflows"))?;
        SnapshotMemory::from_flat(
            memory,
            SandboxMemoryLayout::BASE_ADDRESS as u64,
            data_len,
            data_len..page_table_end,
            scratch_base_gpa(layout.get_scratch_size()),
        )
    }

    /// Create a new snapshot from the guest binary identified by `env`. With the configuration
    /// specified in `cfg`.
    pub(crate) fn from_env<'b>(
        env: impl Into<GuestEnvironment<'b>>,
        cfg: SandboxConfiguration,
    ) -> Result<Self> {
        let env = env.into();
        let mut bin = env.guest_binary;
        bin.canonicalize()?;
        let blob = env.init_data;

        let exe_info = match bin {
            GuestBinary::FilePath(bin_path) => ExeInfo::from_file(&bin_path)?,
            GuestBinary::Buffer(buffer) => ExeInfo::from_buf(buffer)?,
        };

        // Check guest/host version compatibility.
        let host_version = env!("CARGO_PKG_VERSION");
        if let Some(v) = exe_info.guest_bin_version()
            && v != host_version
        {
            return Err(crate::HyperlightError::GuestBinVersionMismatch {
                guest_bin_version: v.to_string(),
                host_version: host_version.to_string(),
            });
        }

        let guest_blob_size = blob.as_ref().map(|b| b.data.len()).unwrap_or(0);
        let guest_blob_mem_flags = blob.as_ref().map(|b| b.permissions);

        let layout = crate::mem::layout::SandboxMemoryLayout::new(
            cfg,
            exe_info.loaded_size(),
            guest_blob_size,
            guest_blob_mem_flags,
        )?;

        let load_addr = layout.get_guest_code_address() as u64;
        let base_va = exe_info.base_va();
        let entrypoint_va: u64 = exe_info.entrypoint().into();

        let mut memory = vec![0; layout.get_memory_size()?];

        let load_info = exe_info.load(
            load_addr.try_into()?,
            &mut memory[layout.guest_code_offset()..],
        )?;

        layout.write_peb(&mut memory)?;

        blob.map(|x| layout.write_init_data(&mut memory, x.data))
            .transpose()?;

        // Set up page table entries for the snapshot
        let pt_buf = GuestPageTableBuffer::new(layout.get_pt_base_gpa() as usize);

        // 1. Map the (ideally readonly) pages of snapshot data
        for rgn in layout.get_memory_regions_::<GuestMemoryRegion>(())?.iter() {
            let readable = rgn.flags.contains(MemoryRegionFlags::READ);
            let executable = rgn.flags.contains(MemoryRegionFlags::EXECUTE);
            let writable = rgn.flags.contains(MemoryRegionFlags::WRITE);
            let kind = if writable {
                MappingKind::Cow(CowMapping {
                    readable,
                    executable,
                })
            } else {
                MappingKind::Basic(BasicMapping {
                    readable,
                    writable: false,
                    executable,
                })
            };
            let mapping = Mapping {
                phys_base: rgn.guest_region.start as u64,
                virt_base: rgn.guest_region.start as u64,
                len: rgn.guest_region.len() as u64,
                kind,
            };
            unsafe { vmem::map(&pt_buf, mapping) };
        }

        // 2. Map the special mappings
        map_specials(&pt_buf, layout.get_scratch_size());

        let data_len = memory.len();
        let pt_bytes = pt_buf.into_bytes();
        layout.ensure_page_tables_fit(pt_bytes.len())?;
        memory.extend(&pt_bytes);

        let exn_stack_top_gva = hyperlight_common::layout::SCRATCH_TOP_GVA as u64
            - hyperlight_common::layout::SCRATCH_TOP_EXN_STACK_OFFSET
            + 1;

        let entrypoint_gva = load_addr + entrypoint_va - base_va;

        let storage = ReadonlySharedMemory::from_bytes(&memory)?;
        let memory = Arc::new(Self::flat_snapshot_memory(
            storage,
            &layout,
            data_len,
            pt_bytes.len(),
        )?);

        Ok(Self {
            memory,
            layout,
            load_info,
            stack_top_gva: exn_stack_top_gva,
            sregs: None,
            #[cfg(target_arch = "x86_64")]
            msrs: None,
            next_action: NextAction::Initialise(entrypoint_gva),
            original_entrypoint: entrypoint_gva,
            snapshot_generation: 0,
            host_functions: HostFunctionDetails {
                host_functions: None,
            },
        })
    }

    // It might be nice to consider moving at least stack_top_gva into
    // layout, and sharing (via RwLock or similar) the layout between
    // the (host-side) mem mgr (where it can be passed in here) and
    // the sandbox vm itself (which modifies it as it receives
    // requests from the sandbox).
    #[allow(clippy::too_many_arguments)]
    /// Take a snapshot of the memory in `shared_mem`, then create a new
    /// instance of `Self` with the snapshot stored therein.
    #[instrument(err(Debug), skip_all, parent = Span::current(), level= "Trace")]
    pub(crate) fn new(
        shared_mem: &SnapshotMemoryBacking<HostSharedMemory>,
        scratch_mem: &HostSharedMemory,
        layout: SandboxMemoryLayout,
        load_info: LoadInfo,
        regions: Vec<MemoryRegion>,
        root_pt_gpas: &[u64],
        stack_top_gva: u64,
        sregs: CommonSpecialRegisters,
        #[cfg(target_arch = "x86_64")] msrs: Vec<MsrEntry>,
        next_action: NextAction,
        original_entrypoint: u64,
        snapshot_generation: u64,
        host_functions: HostFunctionDetails,
    ) -> Result<Self> {
        if root_pt_gpas.is_empty() {
            return Err(crate::new_error!("snapshot has no page-table roots"));
        }
        let mut phys_seen = HashMap::<u64, usize>::new();
        let scratch_gva = scratch_base_gva(layout.get_scratch_size());
        let memory_view = GuestPhysicalMemoryView::new(shared_mem, scratch_mem, &regions, layout);
        let memory = {
            // Phase 1: walk every PT root together. This detects
            // aliased intermediate tables (e.g. Nanvix's kernel-
            // half PTs, which multiple process PDs share by
            // pointing at the same PT page). The walker emits
            // `ThisSpace(leaf)` for private leaves and
            // `AnotherSpace(ref)` for sub-trees that were already
            // seen via an earlier root. Results are returned in
            // `root_pt_gpas` order — which is also the topological
            // order of the `AnotherSpace` references — so
            // processing in iteration order is safe.
            let op = PageTableReader::new(&memory_view, root_pt_gpas[0]);
            let walk = unsafe {
                vmem::walk_va_spaces(
                    &op,
                    root_pt_gpas,
                    0,
                    hyperlight_common::layout::SCRATCH_TOP_GVA as u64,
                )
            };
            op.finish()?;

            // Phase 2: rebuild each space's page tables, compacting
            // `ThisSpace` leaves into a dense snapshot blob and
            // linking `AnotherSpace` entries to already-built
            // spaces' tables.
            // TODO: Look for opportunities to hugepage map
            let mut snapshot_memory: Vec<u8> = Vec::new();
            let pt_buf = GuestPageTableBuffer::new(layout.get_pt_base_gpa() as usize);
            // Allocate one root table per space and remember the
            // addresses returned by `alloc_table` instead of
            // assuming the buffer's physical layout.
            let mut root_addrs: Vec<u64> = Vec::with_capacity(root_pt_gpas.len());
            root_addrs.push(pt_buf.initial_root());
            for _ in 1..root_pt_gpas.len() {
                root_addrs.push(unsafe { pt_buf.alloc_table() });
            }

            let mut built_roots: BTreeMap<SpaceId, u64> = BTreeMap::new();
            for (root_idx, (space_id, mappings)) in walk.into_iter().enumerate() {
                pt_buf.set_root(root_addrs[root_idx]);
                built_roots.insert(space_id, root_addrs[root_idx]);

                for sam in mappings {
                    match sam {
                        SpaceAwareMapping::ThisSpace(mapping) => {
                            // Drop the scratch region and (on
                            // amd64) the snapshot's own PT
                            // self-map; both are re-mapped
                            // freshly by `map_specials`.
                            if skip_virt(mapping.virt_base, scratch_gva) {
                                continue;
                            }
                            let mut contents = [0u8; PAGE_SIZE];
                            if memory_view.read(mapping.phys_base, &mut contents).is_err() {
                                continue;
                            }

                            // Writable pages become CoW in the
                            // rebuilt snapshot; read-only pages
                            // stay read-only.
                            let kind = match mapping.kind {
                                MappingKind::Cow(cm) => MappingKind::Cow(cm),
                                MappingKind::Basic(bm) if bm.writable => {
                                    MappingKind::Cow(CowMapping {
                                        readable: bm.readable,
                                        executable: bm.executable,
                                    })
                                }
                                MappingKind::Basic(bm) => MappingKind::Basic(BasicMapping {
                                    readable: bm.readable,
                                    writable: false,
                                    executable: bm.executable,
                                }),
                                MappingKind::Unmapped => continue,
                            };
                            let new_gpa = phys_seen.entry(mapping.phys_base).or_insert_with(|| {
                                let new_offset = snapshot_memory.len();
                                snapshot_memory.extend(&contents);
                                new_offset + SandboxMemoryLayout::BASE_ADDRESS
                            });

                            let compacted = Mapping {
                                phys_base: *new_gpa as u64,
                                virt_base: mapping.virt_base,
                                len: PAGE_SIZE as u64,
                                kind,
                            };
                            unsafe { vmem::map(&pt_buf, compacted) };
                        }
                        SpaceAwareMapping::AnotherSpace(ref_map) => {
                            // Link to the owning space's already-
                            // rebuilt intermediate table — this
                            // is what preserves Nanvix's
                            // kernel-half-shared invariant across
                            // process PDs after relocation.
                            unsafe {
                                vmem::space_aware_map(&pt_buf, ref_map, &built_roots);
                            }
                        }
                    }
                }
            }

            // Phase 3: Map the scratch region into each root.
            for &root_addr in &root_addrs {
                pt_buf.set_root(root_addr);
                map_specials(&pt_buf, layout.get_scratch_size());
            }
            pt_buf.set_root(pt_buf.initial_root());

            snapshot_memory.resize(
                snapshot_memory.len().next_multiple_of(page_size::get()),
                0u8,
            );

            // Phase 4: finalize PT bytes.
            let pt_data = pt_buf.into_bytes();
            layout.ensure_page_tables_fit(pt_data.len())?;
            let page_table_len = pt_data.len();
            snapshot_memory.extend(&pt_data);
            (snapshot_memory, page_table_len)
        };
        // Only the data prefix is exposed to the guest. The PT tail
        // sits past it in the host mapping and is copied into the
        // scratch region on restore. Keeping it out of the guest
        // mapping of the snapshot region avoids overlap with
        // `map_file_cow` regions installed immediately after the
        // snapshot in guest PA space.
        let (memory, page_table_len) = memory;
        let guest_visible_size = memory.len() - page_table_len;
        debug_assert!(guest_visible_size.is_multiple_of(page_size::get()));
        let storage = ReadonlySharedMemory::from_bytes(&memory)?;
        let memory = Arc::new(Self::flat_snapshot_memory(
            storage,
            &layout,
            guest_visible_size,
            page_table_len,
        )?);

        Ok(Self {
            layout,
            memory,
            load_info,
            stack_top_gva,
            sregs: Some(sregs),
            #[cfg(target_arch = "x86_64")]
            msrs: Some(msrs),
            next_action,
            original_entrypoint,
            snapshot_generation,
            host_functions,
        })
    }

    /// Generation number assigned to this snapshot when it was taken.
    pub(crate) fn snapshot_generation(&self) -> u64 {
        self.snapshot_generation
    }

    /// Return the main memory contents of the snapshot
    #[instrument(skip_all, parent = Span::current(), level= "Trace")]
    pub(crate) fn snapshot_memory(&self) -> &Arc<SnapshotMemory> {
        &self.memory
    }

    /// Return a copy of the load info for the exe in the snapshot
    pub(crate) fn load_info(&self) -> LoadInfo {
        self.load_info.clone()
    }

    pub(crate) fn layout(&self) -> &crate::mem::layout::SandboxMemoryLayout {
        &self.layout
    }

    pub(crate) fn root_pt_gpa(&self) -> u64 {
        self.layout.get_pt_base_gpa()
    }

    pub(crate) fn stack_top_gva(&self) -> u64 {
        self.stack_top_gva
    }

    /// Returns the special registers stored in this snapshot.
    /// Returns None for snapshots created directly from a binary (before preinitialisation).
    /// Returns Some for snapshots taken from a running sandbox.
    /// Note: The CR3 value in the returned struct should NOT be used for restore;
    /// use `root_pt_gpa()` instead since page tables are relocated during snapshot.
    pub(crate) fn sregs(&self) -> Option<&CommonSpecialRegisters> {
        self.sregs.as_ref()
    }

    /// The MSRs saved in this snapshot.
    #[cfg(target_arch = "x86_64")]
    pub(crate) fn msrs(&self) -> Option<&Vec<MsrEntry>> {
        self.msrs.as_ref()
    }

    pub(crate) fn next_action(&self) -> NextAction {
        self.next_action
    }

    /// Guest virtual address of the guest binary's ELF entry point,
    /// preserved across the `Initialise` -> `Call` transition. Used
    /// to fill `AT_ENTRY` in guest core dumps. 0 if unknown.
    pub(crate) fn original_entrypoint(&self) -> u64 {
        self.original_entrypoint
    }

    /// Validate that `provided` is a superset of the host functions
    /// recorded in this snapshot: every function that was registered
    /// at snapshot time must also be present in `provided` with a
    /// matching signature. Extras in `provided` are allowed.
    ///
    /// A snapshot with no recorded host functions (e.g. one
    /// produced by a test-only constructor) accepts any `provided`
    /// set.
    pub(crate) fn validate_host_functions(
        &self,
        provided: &crate::sandbox::host_funcs::FunctionRegistry,
    ) -> Result<()> {
        let required = match &self.host_functions.host_functions {
            Some(v) => v,
            None => return Ok(()),
        };
        if required.is_empty() {
            return Ok(());
        }

        let mut missing: Vec<String> = Vec::new();
        let mut signature_mismatches: Vec<String> = Vec::new();

        for req in required {
            match provided.function_signature(&req.function_name) {
                // Function name is absent from the provided registry.
                None => missing.push(req.function_name.clone()),
                // Function exists, but signature does not match.
                Some((found_parameter_types, found_return_type))
                    if {
                        let params_match = match req.parameter_types.as_deref() {
                            Some(params) => params == found_parameter_types,
                            None => found_parameter_types.is_empty(),
                        };
                        !params_match || req.return_type != found_return_type
                    } =>
                {
                    signature_mismatches.push(format!(
                        "{}: snapshot has {:?} -> {:?}, registered {:?} -> {:?}",
                        req.function_name,
                        req.parameter_types,
                        req.return_type,
                        Some(found_parameter_types.to_vec()),
                        found_return_type,
                    ));
                }
                // Function exists and signature matches.
                Some(_) => {}
            }
        }

        if missing.is_empty() && signature_mismatches.is_empty() {
            return Ok(());
        }

        Err(crate::HyperlightError::SnapshotHostFunctionMismatch {
            missing,
            signature_mismatches,
        })
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use hyperlight_common::flatbuffer_wrappers::host_function_details::HostFunctionDetails;
    use hyperlight_common::vmem::{self, BasicMapping, Mapping, MappingKind, PAGE_SIZE};

    use super::SnapshotMemoryBacking;
    use crate::hypervisor::regs::CommonSpecialRegisters;
    use crate::mem::exe::LoadInfo;
    use crate::mem::layout::SandboxMemoryLayout;
    use crate::mem::mgr::{GuestPageTableBuffer, SandboxMemoryManager};
    use crate::mem::shared_mem::{ExclusiveSharedMemory, HostSharedMemory, ReadonlySharedMemory};

    fn default_sregs() -> CommonSpecialRegisters {
        CommonSpecialRegisters::default()
    }

    #[test]
    fn partial_host_pages_move_to_the_new_blob() {
        let host_page_size = 4 * PAGE_SIZE;
        let blob_gpa = SandboxMemoryLayout::BASE_ADDRESS as u64;
        let mut reused = vec![true, true, false, false, true, true, true, true];
        let mut copied = Vec::new();

        super::move_partial_host_pages(0, &mut reused, blob_gpa, host_page_size, &mut copied)
            .unwrap();

        assert_eq!(
            reused,
            vec![false, false, false, false, true, true, true, true]
        );
        assert_eq!(
            copied
                .iter()
                .map(|page| page.source_gpa)
                .collect::<Vec<_>>(),
            vec![blob_gpa, blob_gpa + PAGE_SIZE as u64]
        );
        assert_eq!(
            super::packed_data_len(copied.len(), host_page_size).unwrap(),
            host_page_size
        );
    }

    #[test]
    fn partial_host_pages_move_from_the_final_group() {
        let host_page_size = 4 * PAGE_SIZE;
        let blob_gpa = SandboxMemoryLayout::BASE_ADDRESS as u64;
        let mut reused = vec![true, true, true, true, true, false, true, false];
        let mut copied = Vec::new();

        super::move_partial_host_pages(0, &mut reused, blob_gpa, host_page_size, &mut copied)
            .unwrap();

        assert_eq!(
            reused,
            vec![true, true, true, true, false, false, false, false]
        );
        assert_eq!(
            copied
                .iter()
                .map(|page| page.source_gpa)
                .collect::<Vec<_>>(),
            vec![
                blob_gpa + 4 * PAGE_SIZE as u64,
                blob_gpa + 6 * PAGE_SIZE as u64
            ]
        );
    }

    /// Demotion splits one live run into several, and every run costs a
    /// mapping. A layer that fits the cap on a 4K host can exceed it on a
    /// host with larger pages.
    #[test]
    fn partial_host_pages_fragment_live_ranges() {
        let host_page_size = 4 * PAGE_SIZE;
        let blob_gpa = SandboxMemoryLayout::BASE_ADDRESS as u64;
        let mut reused = vec![true; 16];
        // Dirty one guest page in every second host page.
        for index in [1, 9] {
            reused[index] = false;
        }
        assert_eq!(super::coalesce_pages(&reused).len(), 3);

        let mut copied = Vec::new();
        super::move_partial_host_pages(0, &mut reused, blob_gpa, host_page_size, &mut copied)
            .unwrap();

        // The two touched host pages lose every page they held, leaving the
        // untouched host pages either side of them.
        assert_eq!(super::coalesce_pages(&reused).len(), 2);
        assert_eq!(copied.len(), 6);
    }

    fn new_page(source_gpa: u64, offset: usize) -> super::NewPage {
        super::NewPage {
            source_gpa,
            backing: GuestBacking::Snapshot {
                layer_index: 0,
                offset,
            },
        }
    }

    #[test]
    fn page_runs_map_each_source_to_its_destination() {
        let base = SandboxMemoryLayout::BASE_ADDRESS as u64;
        let page = PAGE_SIZE as u64;
        let destination = base + 0x10_0000;
        // Two contiguous pages, then a gap, then one more.
        let pages = [
            new_page(base, 0),
            new_page(base + page, PAGE_SIZE),
            new_page(base + 8 * page, 8 * PAGE_SIZE),
        ];

        let runs = super::new_page_runs(&pages, destination).unwrap();
        assert_eq!(runs.len(), 2);

        // Destinations are packed in index order regardless of source gaps.
        assert_eq!(super::page_destination(&runs, base), Some(destination));
        assert_eq!(
            super::page_destination(&runs, base + page),
            Some(destination + page)
        );
        assert_eq!(
            super::page_destination(&runs, base + 8 * page),
            Some(destination + 2 * page)
        );
        // An address in the source gap belongs to no run.
        assert_eq!(super::page_destination(&runs, base + 2 * page), None);
        assert_eq!(super::page_destination(&runs, base + 9 * page), None);
    }

    #[test]
    fn page_copies_merge_only_contiguous_backings() {
        let base = SandboxMemoryLayout::BASE_ADDRESS as u64;
        let page = PAGE_SIZE as u64;
        // Contiguous source addresses, but the third page comes from a
        // backing offset that does not follow the second.
        let pages = [
            new_page(base, 0),
            new_page(base + page, PAGE_SIZE),
            new_page(base + 2 * page, 9 * PAGE_SIZE),
        ];

        let copies = super::new_page_copies(&pages).unwrap();
        assert_eq!(copies.len(), 2);
        assert_eq!(copies[0].destination, 0..2 * PAGE_SIZE);
        assert_eq!(copies[1].destination, 2 * PAGE_SIZE..3 * PAGE_SIZE);
    }

    #[test]
    fn blob_placement_respects_the_scratch_boundary() {
        let base = SandboxMemoryLayout::BASE_ADDRESS as u64;
        let scratch_base = base + 2 * PAGE_SIZE as u64;

        // An empty delta needs no address.
        assert_eq!(super::first_fit_blob_gpa(0, &[], scratch_base), None);
        // With nothing to reuse the delta lands at the base.
        assert_eq!(
            super::first_fit_blob_gpa(PAGE_SIZE, &[], scratch_base),
            Some(base)
        );
        // Exactly filling the space below scratch is allowed.
        assert_eq!(
            super::first_fit_blob_gpa(2 * PAGE_SIZE, &[], scratch_base),
            Some(base)
        );
        // One page more is not.
        assert_eq!(
            super::first_fit_blob_gpa(3 * PAGE_SIZE, &[], scratch_base),
            None
        );
    }

    #[test]
    fn guest_sized_host_pages_need_no_regrouping() {
        let blob_gpa = SandboxMemoryLayout::BASE_ADDRESS as u64;
        let expected = vec![true, true, false, true];
        let mut reused = expected.clone();
        let mut copied = Vec::new();

        super::move_partial_host_pages(0, &mut reused, blob_gpa, PAGE_SIZE, &mut copied).unwrap();

        assert_eq!(reused, expected);
        assert!(copied.is_empty());
    }

    #[test]
    fn snapshot_page_policy_reuses_shared_and_copies_private_backings() {
        let source_gpa = SandboxMemoryLayout::BASE_ADDRESS as u64;
        let backing = GuestBacking::Snapshot {
            layer_index: 0,
            offset: 0,
        };
        let mut reused = vec![vec![false; 2]];
        let mut copied = Vec::new();

        super::record_backed_pages(
            backing,
            source_gpa,
            2 * PAGE_SIZE,
            Some(reused.as_mut_slice()),
            &mut copied,
        )
        .unwrap();
        assert_eq!(reused, [vec![true; 2]]);
        assert!(copied.is_empty());

        reused[0].fill(false);
        super::record_backed_pages(backing, source_gpa, 2 * PAGE_SIZE, None, &mut copied).unwrap();
        assert_eq!(reused, [vec![false; 2]]);
        assert_eq!(copied.len(), 2);
        assert_eq!(copied[0].source_gpa, source_gpa);
        assert_eq!(copied[0].backing, backing);
        assert_eq!(copied[1].source_gpa, source_gpa + PAGE_SIZE as u64);
        assert_eq!(
            copied[1].backing,
            GuestBacking::Snapshot {
                layer_index: 0,
                offset: PAGE_SIZE,
            }
        );
    }

    fn make_simple_pt_memory(contents: &[u8], pt_base: u64) -> super::SnapshotMemory {
        let pt_buf = GuestPageTableBuffer::new(pt_base as usize);
        let mapping = Mapping {
            phys_base: SandboxMemoryLayout::BASE_ADDRESS as u64,
            virt_base: SandboxMemoryLayout::BASE_ADDRESS as u64,
            len: page_size::get() as u64,
            kind: MappingKind::Basic(BasicMapping {
                readable: true,
                writable: true,
                executable: true,
            }),
        };
        unsafe { vmem::map(&pt_buf, mapping) };
        super::map_specials(&pt_buf, PAGE_SIZE);
        let pt_bytes = pt_buf.into_bytes();

        let mut snapshot_mem = vec![0u8; page_size::get() + pt_bytes.len()];
        snapshot_mem[0..page_size::get()].copy_from_slice(contents);
        snapshot_mem[page_size::get()..].copy_from_slice(&pt_bytes);
        let storage = ReadonlySharedMemory::from_bytes(&snapshot_mem).unwrap();
        super::SnapshotMemory::from_flat(
            storage,
            SandboxMemoryLayout::BASE_ADDRESS as u64,
            page_size::get(),
            page_size::get()..snapshot_mem.len(),
            hyperlight_common::layout::scratch_base_gpa(PAGE_SIZE),
        )
        .unwrap()
    }

    fn make_simple_pt_host_mem(
        contents: &[u8],
        pt_base: u64,
    ) -> SnapshotMemoryBacking<HostSharedMemory> {
        SnapshotMemoryBacking::from_snapshot(Arc::new(make_simple_pt_memory(contents, pt_base)))
            .unwrap()
            .build()
            .0
    }

    fn make_simple_pt_mgr() -> (SandboxMemoryManager<HostSharedMemory>, u64) {
        let cfg = crate::sandbox::SandboxConfiguration::default();
        let scratch_mem = ExclusiveSharedMemory::new(cfg.get_scratch_size()).unwrap();
        let layout = SandboxMemoryLayout::new(cfg, 4096, 0x3000, None).unwrap();
        let pt_base = layout.get_pt_base_gpa();
        let memory = make_simple_pt_memory(&vec![0u8; page_size::get()], pt_base);
        layout
            .ensure_page_tables_fit(memory.page_table_len())
            .unwrap();
        let mgr = SandboxMemoryManager::new(
            layout,
            SnapshotMemoryBacking::from_snapshot(Arc::new(memory)).unwrap(),
            scratch_mem,
            super::NextAction::None,
        );
        let (mgr, _) = mgr.build().unwrap();
        (mgr, pt_base)
    }

    #[test]
    fn multiple_snapshots_independent() {
        let (mut mgr, pt_base) = make_simple_pt_mgr();

        // Create first snapshot with pattern A
        let pattern_a = vec![0xAA; page_size::get()];
        let pattern_a_memory = make_simple_pt_host_mem(&pattern_a, pt_base);
        let snapshot_a = super::Snapshot::new(
            &pattern_a_memory,
            &mgr.scratch_mem,
            mgr.layout,
            LoadInfo::dummy(),
            Vec::new(),
            &[pt_base],
            0,
            default_sregs(),
            #[cfg(target_arch = "x86_64")]
            Vec::new(),
            super::NextAction::None,
            0,
            1,
            HostFunctionDetails::default(),
        )
        .unwrap();

        // Create second snapshot with pattern B
        let pattern_b = vec![0xBB; page_size::get()];
        let pattern_b_memory = make_simple_pt_host_mem(&pattern_b, pt_base);
        let snapshot_b = super::Snapshot::new(
            &pattern_b_memory,
            &mgr.scratch_mem,
            mgr.layout,
            LoadInfo::dummy(),
            Vec::new(),
            &[pt_base],
            0,
            default_sregs(),
            #[cfg(target_arch = "x86_64")]
            Vec::new(),
            super::NextAction::None,
            0,
            2,
            HostFunctionDetails::default(),
        )
        .unwrap();

        // Restore snapshot A
        mgr.restore_snapshot(&snapshot_a).unwrap();
        let mut restored = vec![0u8; pattern_a.len()];
        mgr.shared_mem
            .read_snapshot_gpa(SandboxMemoryLayout::BASE_ADDRESS as u64, &mut restored)
            .unwrap();
        assert_eq!(restored, pattern_a);

        // Restore snapshot B
        mgr.restore_snapshot(&snapshot_b).unwrap();
        restored.fill(0);
        mgr.shared_mem
            .read_snapshot_gpa(SandboxMemoryLayout::BASE_ADDRESS as u64, &mut restored)
            .unwrap();
        assert_eq!(restored, pattern_b);
    }

    #[test]
    fn page_table_reader_reports_a_suppressed_failure() {
        let (manager, pt_base) = make_simple_pt_mgr();
        let memory = crate::mem::mgr::GuestPhysicalMemoryView::new(
            &manager.shared_mem,
            &manager.scratch_mem,
            &[],
            manager.layout,
        );

        let reader = super::PageTableReader::new(&memory, pt_base);
        assert!(reader.finish().is_ok());

        // A read off every backing is recorded rather than returned, so the
        // walk keeps going against not-present entries until finish() asks.
        // SAFETY: the reader borrows `memory`, which outlives this call.
        let entry = unsafe {
            <super::PageTableReader as vmem::TableReadOps>::read_entry(
                &reader,
                !(PAGE_SIZE as u64 - 1),
            )
        };
        assert_eq!(entry, 0);
        let error = reader.finish().unwrap_err().to_string();
        assert!(error.contains("accessed unbacked memory"), "{error}");
    }
}
