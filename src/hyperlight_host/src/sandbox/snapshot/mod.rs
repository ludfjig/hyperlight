// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 The Hyperlight Authors.

mod file;
mod file_tests;
pub(crate) mod memory;
mod tripwires;

use std::cell::Cell;
use std::collections::{BTreeMap, HashMap, HashSet};
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
pub(crate) use self::memory::{
    SnapshotBlob, SnapshotMemory, SnapshotMemoryBacking, SnapshotPageTables,
};
use crate::Result;
use crate::hypervisor::regs::CommonSpecialRegisters;
#[cfg(target_arch = "x86_64")]
use crate::hypervisor::regs::MsrEntry;
use crate::mem::exe::{ExeInfo, LoadInfo};
use crate::mem::layout::SandboxMemoryLayout;
use crate::mem::memory_region::{MemoryRegion, MemoryRegionFlags};
use crate::mem::mgr::{GuestPageTableBuffer, GuestPhysicalMemoryView};
use crate::mem::shared_mem::{ExclusiveSharedMemory, HostSharedMemory, ReadonlySharedMemory};
use crate::mem::virtq::VirtqSnapshot;
use crate::sandbox::SandboxConfiguration;
use crate::sandbox::uninitialized::{GuestBinary, GuestEnvironment};

const PTE_SIZE: usize = size_of::<vmem::PageTableEntry>();
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

    /// Returns suppressed page-table read failures.
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

/// An immutable snapshot of sandbox state.
pub struct Snapshot {
    state: Arc<SnapshotState>,
    // Stable key order keeps config bytes deterministic.
    metadata: BTreeMap<String, serde_json::Value>,
}

/// Immutable sandbox state held by a snapshot.
struct SnapshotState {
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
    /// (`code GVA + e_entry - base_va`). Unlike `next_action`, which
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
    /// [`crate::Sandbox::from_snapshot`] to reject a
    /// `HostFunctions` set that is missing required functions or
    /// has mismatched signatures.
    host_functions: HostFunctionDetails,

    /// Validated ring images omitted from ordinary snapshot pages.
    ///
    /// Construction and loading validate these images against `layout`.
    /// Both the images and layout remain immutable afterwards.
    virtq: Option<VirtqSnapshot>,
}

impl core::convert::AsRef<Snapshot> for Snapshot {
    fn as_ref(&self) -> &Self {
        self
    }
}
// Loaded snapshots serve fixed-depth translations, so each operation has an
// architectural read bound and needs no shared walk budget.
impl hyperlight_common::vmem::TableReadOps for Snapshot {
    type TableAddr = u64;
    fn entry_addr(addr: u64, offset: u64) -> u64 {
        addr.saturating_add(offset)
    }
    unsafe fn read_entry(&self, addr: u64) -> vmem::PageTableEntry {
        let mut pte_bytes = [0u8; PTE_SIZE];
        if self
            .state
            .memory
            .read_page_tables(self.state.layout.get_pt_base_gpa(), addr, &mut pte_bytes)
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
    /// Split a flat image, guest data followed by page tables, into the layered
    /// form. The two parts need separate allocations, so this copies.
    fn flat_snapshot_memory(
        memory: ReadonlySharedMemory,
        layout: &SandboxMemoryLayout,
        data_len: usize,
        page_table_len: usize,
    ) -> Result<SnapshotMemory> {
        let page_table_end = data_len
            .checked_add(page_table_len)
            .ok_or_else(|| crate::new_error!("snapshot memory size overflows"))?;
        let bytes = memory.as_slice();
        let page_tables = bytes
            .get(data_len..page_table_end)
            .ok_or_else(|| crate::new_error!("snapshot page tables are out of bounds"))?;
        let page_tables = Arc::new(SnapshotPageTables::new(
            ReadonlySharedMemory::from_bytes(page_tables)?,
            page_table_len,
        )?);
        let data = bytes
            .get(..data_len)
            .ok_or_else(|| crate::new_error!("snapshot data is out of bounds"))?;
        SnapshotMemory::from_flat(
            ReadonlySharedMemory::from_bytes(data)?,
            SandboxMemoryLayout::BASE_ADDRESS as u64,
            data_len,
            page_tables,
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

        let mut layout = crate::mem::layout::SandboxMemoryLayout::new(
            cfg,
            exe_info.loaded_size(),
            guest_blob_size,
            guest_blob_mem_flags,
        )?;

        let load_addr = layout.get_guest_code_gpa() as u64;
        let base_va = exe_info.base_va();
        let entrypoint_va: u64 = exe_info.entrypoint().into();
        let is_pie = exe_info.is_pie();

        let code_gva = if is_pie { load_addr } else { base_va };
        layout.set_code_gva(code_gva)?;
        let regions = layout.get_memory_regions()?;

        let data_len = layout.get_memory_size()?;
        let mut memory = ExclusiveSharedMemory::new(data_len)?;

        let load_info = exe_info.load(
            layout.get_guest_code_gva() as u64,
            &mut memory.as_mut_slice()[layout.guest_code_offset()..],
        )?;

        layout.write_peb(memory.as_mut_slice())?;

        blob.map(|x| layout.write_init_data(memory.as_mut_slice(), x.data))
            .transpose()?;

        // Set up page table entries for the snapshot
        let pt_buf = GuestPageTableBuffer::new(layout.get_pt_base_gpa() as usize);

        // 1. Map the (ideally readonly) pages of snapshot data
        for rgn in regions.iter() {
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
                phys_base: rgn.host_region.start as u64,
                virt_base: rgn.guest_region.start as u64,
                len: rgn.guest_region.len() as u64,
                kind,
            };
            unsafe { vmem::map(&pt_buf, mapping) };
        }

        // 2. Map the special mappings
        map_specials(&pt_buf, layout.get_scratch_size());

        let pt_bytes = pt_buf.into_bytes();
        layout.ensure_page_tables_fit(pt_bytes.len())?;

        let exn_stack_top_gva = hyperlight_common::layout::SCRATCH_TOP_GVA as u64
            - hyperlight_common::layout::SCRATCH_TOP_EXN_STACK_OFFSET
            + 1;

        let entrypoint_offset = entrypoint_va.checked_sub(base_va).ok_or_else(|| {
            crate::new_error!(
                "ELF entrypoint VA ({:#x}) is below base VA ({:#x})",
                entrypoint_va,
                base_va
            )
        })?;

        let entrypoint_gva = (layout.get_guest_code_gva() as u64)
            .checked_add(entrypoint_offset)
            .ok_or_else(|| crate::new_error!("ELF entrypoint GVA overflows"))?;

        let page_tables = Arc::new(SnapshotPageTables::new(
            ReadonlySharedMemory::from_bytes(&pt_bytes)?,
            pt_bytes.len(),
        )?);
        let memory = Arc::new(SnapshotMemory::from_flat(
            memory.freeze()?,
            SandboxMemoryLayout::BASE_ADDRESS as u64,
            data_len,
            page_tables,
            scratch_base_gpa(layout.get_scratch_size()),
        )?);

        Ok(Self {
            state: Arc::new(SnapshotState {
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
                virtq: None,
            }),
            metadata: BTreeMap::new(),
        })
    }

    // It might be nice to consider moving at least stack_top_gva into
    // layout, and sharing (via RwLock or similar) the layout between
    // the (host-side) mem mgr (where it can be passed in here) and
    // the sandbox vm itself (which modifies it as it receives
    // requests from the sandbox).
    #[allow(clippy::too_many_arguments)]
    /// Capture memory with optional validated transport images.
    ///
    /// Page-table and snapshot sizes do not affect transport geometry.
    ///
    /// # Safety
    /// Each region's host range must remain allocated and readable for this
    /// call. The vCPU and other writers must not mutate the ranges while they
    /// are read.
    #[instrument(err(Debug), skip_all, parent = Span::current(), level= "Trace")]
    pub(crate) unsafe fn new(
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
        virtq: Option<VirtqSnapshot>,
    ) -> Result<Self> {
        if root_pt_gpas.is_empty() {
            return Err(crate::new_error!("snapshot has no page-table roots"));
        }
        let mut phys_seen = HashMap::<u64, usize>::new();
        let scratch_gva = scratch_base_gva(layout.get_scratch_size());
        // SAFETY: The caller upholds the mapped-region lifetime and nonmutation
        // requirements for this call.
        let memory_view = unsafe {
            GuestPhysicalMemoryView::with_dynamic(shared_mem, scratch_mem, &regions, layout)
        };
        let mut roots = HashSet::with_capacity(root_pt_gpas.len());
        for &root in root_pt_gpas {
            if !root.is_multiple_of(PAGE_SIZE as u64) || !roots.insert(root) {
                return Err(crate::new_error!(
                    "snapshot page-table root is invalid: {root:#x}"
                ));
            }
            if memory_view.resolve(root, PAGE_SIZE).is_none() {
                return Err(crate::new_error!(
                    "snapshot page-table root is unbacked: {root:#x}"
                ));
            }
        }
        let (memory, pt_data) = {
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

                            // Writable pages become CoW in the
                            // rebuilt snapshot; read-only pages
                            // stay read-only.
                            let (kind, has_contents) = match mapping.kind {
                                MappingKind::Cow(cm) => (MappingKind::Cow(cm), true),
                                MappingKind::Basic(bm) if bm.writable => (
                                    MappingKind::Cow(CowMapping {
                                        readable: bm.readable,
                                        executable: bm.executable,
                                    }),
                                    true,
                                ),
                                MappingKind::Basic(bm) => (
                                    MappingKind::Basic(BasicMapping {
                                        readable: bm.readable,
                                        writable: false,
                                        executable: bm.executable,
                                    }),
                                    true,
                                ),
                                MappingKind::Unmapped => continue,
                                MappingKind::ZeroInit(bm) => (MappingKind::ZeroInit(bm), false),
                            };
                            let new_gpa = if has_contents {
                                let mut contents = [0u8; PAGE_SIZE];
                                memory_view.read(mapping.phys_base, &mut contents)?;
                                Some(*phys_seen.entry(mapping.phys_base).or_insert_with(|| {
                                    let new_offset = snapshot_memory.len();
                                    snapshot_memory.extend(&contents);
                                    new_offset + SandboxMemoryLayout::BASE_ADDRESS
                                }))
                            } else {
                                None
                            };

                            let compacted = Mapping {
                                phys_base: new_gpa.unwrap_or(0) as u64,
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
            (snapshot_memory, pt_data)
        };
        let page_tables = Arc::new(SnapshotPageTables::new(
            ReadonlySharedMemory::from_bytes(&pt_data)?,
            pt_data.len(),
        )?);
        let memory = Arc::new(SnapshotMemory::from_flat(
            ReadonlySharedMemory::from_bytes(&memory)?,
            SandboxMemoryLayout::BASE_ADDRESS as u64,
            memory.len(),
            page_tables,
            scratch_base_gpa(layout.get_scratch_size()),
        )?);

        Ok(Self {
            state: Arc::new(SnapshotState {
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
                virtq,
            }),
            metadata: BTreeMap::new(),
        })
    }

    /// Deserializes the metadata stored under `namespace` as `T`.
    ///
    /// Returns `None` if the namespace has no metadata.
    ///
    /// # Errors
    ///
    /// Returns an error if the JSON metadata cannot be deserialized as `T`.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// # use std::sync::Arc;
    /// # use hyperlight_host::sandbox::snapshot::Snapshot;
    /// # use serde::{Deserialize, Serialize};
    /// #
    /// #[derive(Debug, PartialEq, Serialize, Deserialize)]
    /// struct Metadata {
    ///     version: u32,
    /// }
    ///
    /// # fn example(snapshot: Arc<Snapshot>) -> Result<(), Box<dyn std::error::Error>> {
    /// let snapshot =
    ///     snapshot.with_metadata("snapshot-metadata-namespace-v1", &Metadata { version: 1 })?;
    /// let metadata = snapshot
    ///     .metadata::<Metadata>("snapshot-metadata-namespace-v1")?
    ///     .expect("metadata should exist");
    /// assert_eq!(metadata, Metadata { version: 1 });
    /// # Ok(())
    /// # }
    /// ```
    pub fn metadata<T>(&self, namespace: &str) -> Result<Option<T>>
    where
        T: serde::de::DeserializeOwned,
    {
        self.metadata
            .get(namespace)
            .map(serde::Deserialize::deserialize)
            .transpose()
            .map_err(Into::into)
    }

    /// Adds `metadata` to this snapshot and returns the result as a new
    /// snapshot. Metadata already stored under `namespace` is replaced.
    ///
    /// This snapshot remains unchanged.
    /// Metadata is saved and loaded with the snapshot.
    ///
    /// # Errors
    ///
    /// Returns an error if `metadata` cannot be serialized as JSON.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// # use std::sync::Arc;
    /// # use hyperlight_host::sandbox::snapshot::Snapshot;
    /// # use serde::{Deserialize, Serialize};
    /// #
    /// #[derive(Debug, PartialEq, Serialize, Deserialize)]
    /// struct Metadata {
    ///     version: u32,
    /// }
    ///
    /// # fn example(snapshot: Arc<Snapshot>) -> Result<(), Box<dyn std::error::Error>> {
    /// let snapshot =
    ///     snapshot.with_metadata("snapshot-metadata-namespace-v1", &Metadata { version: 1 })?;
    /// let metadata = snapshot
    ///     .metadata::<Metadata>("snapshot-metadata-namespace-v1")?
    ///     .expect("metadata should exist");
    /// assert_eq!(metadata, Metadata { version: 1 });
    /// # Ok(())
    /// # }
    /// ```
    pub fn with_metadata<T>(&self, namespace: impl Into<String>, metadata: &T) -> Result<Arc<Self>>
    where
        T: serde::Serialize,
    {
        let namespace = namespace.into();
        let metadata = serde_json::to_value(metadata)?;
        let mut metadata_by_namespace = self.metadata.clone();
        metadata_by_namespace.insert(namespace, metadata);
        Ok(Arc::new(Self {
            state: Arc::clone(&self.state),
            metadata: metadata_by_namespace,
        }))
    }

    /// Generation number assigned to this snapshot when it was taken.
    pub(crate) fn snapshot_generation(&self) -> u64 {
        self.state.snapshot_generation
    }

    /// Return the main memory contents of the snapshot
    #[instrument(skip_all, parent = Span::current(), level= "Trace")]
    pub(crate) fn snapshot_memory(&self) -> &Arc<SnapshotMemory> {
        &self.state.memory
    }

    /// Return a copy of the load info for the exe in the snapshot
    pub(crate) fn load_info(&self) -> LoadInfo {
        self.state.load_info.clone()
    }

    pub(crate) fn layout(&self) -> &crate::mem::layout::SandboxMemoryLayout {
        &self.state.layout
    }

    pub(crate) fn root_pt_gpa(&self) -> u64 {
        self.state.layout.get_pt_base_gpa()
    }

    pub(crate) fn stack_top_gva(&self) -> u64 {
        self.state.stack_top_gva
    }

    /// Returns the special registers stored in this snapshot.
    /// Returns None for snapshots created directly from a binary (before preinitialisation).
    /// Returns Some for snapshots taken from a running sandbox.
    /// Note: The CR3 value in the returned struct should NOT be used for restore;
    /// use `root_pt_gpa()` instead since page tables are relocated during snapshot.
    pub(crate) fn sregs(&self) -> Option<&CommonSpecialRegisters> {
        self.state.sregs.as_ref()
    }

    /// The MSRs saved in this snapshot.
    #[cfg(target_arch = "x86_64")]
    pub(crate) fn msrs(&self) -> Option<&Vec<MsrEntry>> {
        self.state.msrs.as_ref()
    }

    pub(crate) fn next_action(&self) -> NextAction {
        self.state.next_action
    }

    pub(crate) fn virtq(&self) -> Option<&VirtqSnapshot> {
        self.state.virtq.as_ref()
    }

    /// Guest virtual address of the guest binary's ELF entry point,
    /// preserved across the `Initialise` -> `Call` transition. Used
    /// to fill `AT_ENTRY` in guest core dumps. 0 if unknown.
    pub(crate) fn original_entrypoint(&self) -> u64 {
        self.state.original_entrypoint
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
        let required = match &self.state.host_functions.host_functions {
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

        let storage = ReadonlySharedMemory::from_bytes(contents).unwrap();
        let page_tables = Arc::new(
            super::SnapshotPageTables::new(
                ReadonlySharedMemory::from_bytes(&pt_bytes).unwrap(),
                pt_bytes.len(),
            )
            .unwrap(),
        );
        super::SnapshotMemory::from_flat(
            storage,
            SandboxMemoryLayout::BASE_ADDRESS as u64,
            page_size::get(),
            page_tables,
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
        // SAFETY: No dynamic regions are supplied.
        let snapshot_a = unsafe {
            super::Snapshot::new(
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
                None,
            )
        }
        .unwrap();

        // Create second snapshot with pattern B
        let pattern_b = vec![0xBB; page_size::get()];
        let pattern_b_memory = make_simple_pt_host_mem(&pattern_b, pt_base);
        // SAFETY: No dynamic regions are supplied.
        let snapshot_b = unsafe {
            super::Snapshot::new(
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
                None,
            )
        }
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
    fn page_table_reader_reports_unbacked_memory() {
        let (manager, pt_base) = make_simple_pt_mgr();
        let memory = crate::mem::mgr::GuestPhysicalMemoryView::new(
            &manager.shared_mem,
            &manager.scratch_mem,
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
        assert_eq!(
            <super::PageTableReader<'_> as vmem::TableReadOps>::entry_addr(u64::MAX, 8),
            u64::MAX
        );
    }

    #[test]
    fn page_table_reader_reports_exhausted_budget() {
        let (manager, root) = make_simple_pt_mgr();
        let memory = crate::mem::mgr::GuestPhysicalMemoryView::new(
            &manager.shared_mem,
            &manager.scratch_mem,
            manager.layout,
        );
        let reader = super::PageTableReader::new(&memory, root);
        reader.reads.set(super::MAX_SNAPSHOT_PAGE_TABLE_READS);

        // SAFETY: The reader refuses reads after its budget is exhausted.
        let entry = unsafe { vmem::TableReadOps::read_entry(&reader, root) };

        assert_eq!(entry, 0);
        assert!(reader.finish().unwrap_err().to_string().contains("limit"));
    }

    #[test]
    fn capture_rejects_invalid_page_table_roots() {
        let (manager, root) = make_simple_pt_mgr();
        for roots in [
            Vec::new(),
            vec![root + 1],
            vec![root, root],
            vec![u64::MAX - (PAGE_SIZE as u64 - 1)],
        ] {
            // SAFETY: No dynamic regions are supplied and the vCPU is stopped.
            let result = unsafe {
                super::Snapshot::new(
                    &manager.shared_mem,
                    &manager.scratch_mem,
                    manager.layout,
                    LoadInfo::dummy(),
                    Vec::new(),
                    &roots,
                    0,
                    default_sregs(),
                    #[cfg(target_arch = "x86_64")]
                    Vec::new(),
                    super::NextAction::None,
                    0,
                    1,
                    HostFunctionDetails::default(),
                    None,
                )
            };
            assert!(result.is_err(), "invalid roots accepted: {roots:?}");
            assert_eq!(manager.snapshot_count, 0);
        }
    }
}
