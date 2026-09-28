// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 The Hyperlight Authors.

use std::collections::BTreeMap;

use hyperlight_common::flatbuffer_wrappers::function_types::{ParameterType, ReturnType};
use hyperlight_common::flatbuffer_wrappers::host_function_definition::HostFunctionDefinition;
use hyperlight_common::vmem::PAGE_SIZE;
use serde::{Deserialize, Serialize};

use super::media_types::{SNAPSHOT_ABI_VERSION, SNAPSHOT_ABI_VERSION_V4};
use crate::hypervisor::regs::CommonSpecialRegisters;
#[cfg(target_arch = "x86_64")]
use crate::hypervisor::regs::MsrEntry;
use crate::mem::layout::SandboxMemoryLayout;
use crate::sandbox::snapshot::memory::{
    SnapshotLayer, validate_snapshot_blob_layout, validate_snapshot_layer_count,
    validate_snapshot_live_data, validate_snapshot_page_tables, validate_snapshot_totals,
    validate_sorted_snapshot_gpa_ranges,
};

// --- Arch and hypervisor identifiers --------------------------------

/// Guest architecture the snapshot was captured for. Checked on load
/// against the running host.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub(super) enum Arch {
    X86_64,
    Aarch64,
}

impl Arch {
    pub(super) fn current() -> Self {
        #[cfg(target_arch = "x86_64")]
        {
            Self::X86_64
        }
        #[cfg(target_arch = "aarch64")]
        {
            Self::Aarch64
        }
    }

    /// Lowercase token matching the config JSON serialization, used
    /// for the advisory arch annotation on the manifest descriptor.
    pub(super) fn as_str(&self) -> &'static str {
        match self {
            Self::X86_64 => "x86_64",
            Self::Aarch64 => "aarch64",
        }
    }
}

/// Hypervisor backend the snapshot was captured under. Checked on
/// load because vCPU register state is backend-specific.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub(super) enum Hypervisor {
    Kvm,
    Mshv,
    Whp,
    Hvf,
}

impl Hypervisor {
    pub(super) fn current() -> Option<Self> {
        #[allow(unused_imports)]
        use crate::hypervisor::virtual_machine::HypervisorType;
        use crate::hypervisor::virtual_machine::get_available_hypervisor;

        match get_available_hypervisor() {
            #[cfg(kvm)]
            Some(HypervisorType::Kvm) => Some(Self::Kvm),
            #[cfg(mshv3)]
            Some(HypervisorType::Mshv) => Some(Self::Mshv),
            #[cfg(target_os = "windows")]
            Some(HypervisorType::Whp) => Some(Self::Whp),
            #[cfg(hvf)]
            Some(HypervisorType::Hvf) => Some(Self::Hvf),
            None => None,
        }
    }

    fn name(&self) -> &'static str {
        match self {
            Self::Kvm => "KVM",
            Self::Mshv => "MSHV",
            Self::Whp => "WHP",
            Self::Hvf => "HVF",
        }
    }

    /// Lowercase token matching the config JSON serialization, used
    /// for the advisory hypervisor annotation on the manifest
    /// descriptor.
    pub(super) fn as_str(&self) -> &'static str {
        match self {
            Self::Kvm => "kvm",
            Self::Mshv => "mshv",
            Self::Whp => "whp",
            Self::Hvf => "hvf",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(super) struct CpuVendor(String);

impl CpuVendor {
    /// The vendor identifier of the running host.
    pub(super) fn current() -> Self {
        #[cfg(target_arch = "x86_64")]
        {
            // SAFETY: CPUID leaf 0 is always available on x86_64.
            // TODO: Remove the `unsafe`/allow when MSRV is raised above
            // 1.89. On Rust 1.89 `__cpuid` requires `unsafe`; on newer
            // compilers it is safe and clippy flags it as unnecessary.
            #[allow(unused_unsafe)]
            let r = unsafe { core::arch::x86_64::__cpuid(0) };
            let mut bytes = [0u8; 12];
            bytes[0..4].copy_from_slice(&r.ebx.to_le_bytes());
            bytes[4..8].copy_from_slice(&r.edx.to_le_bytes());
            bytes[8..12].copy_from_slice(&r.ecx.to_le_bytes());
            Self(String::from_utf8_lossy(&bytes).into_owned())
        }
        #[cfg(all(target_arch = "aarch64", target_os = "linux"))]
        {
            let midr: u64;
            // SAFETY: Linux emulates MIDR_EL1 reads from EL0.
            unsafe { core::arch::asm!("mrs {}, MIDR_EL1", out(reg) midr) };
            let implementer = (midr >> 24) & 0xff;
            // `0x` prefix padded to width 4, e.g. Apple `0x61`, Arm `0x41`.
            Self(format!("{implementer:#04x}"))
        }
        #[cfg(all(target_arch = "aarch64", target_os = "macos"))]
        {
            Self("0x61".to_string())
        }
    }

    pub(super) fn as_str(&self) -> &str {
        &self.0
    }

    /// Short, stable token naming this vendor for snapshot golden
    /// tags, or `None` for a vendor the goldens do not cover.
    pub(crate) fn golden_tag(&self) -> Option<&'static str> {
        match self.0.as_str() {
            "GenuineIntel" => Some("intel"),
            "AuthenticAMD" => Some("amd"),
            // aarch64 MIDR_EL1 implementer byte for Apple silicon.
            "0x61" => Some("apple"),
            _ => None,
        }
    }
}

// --- Config JSON shape ----------------------------------------------

/// Top-level Hyperlight snapshot config JSON. Lives at
/// `blobs/sha256/<config-digest>` with media type
/// `application/vnd.hyperlight.snapshot.config.v3+json`.
///
/// In OCI terms this is the "image config" blob that the manifest's
/// `config` descriptor points to. It describes the accompanying
/// memory layer (the snapshot bytes) and everything the loader needs
/// to reconstruct a runnable `Snapshot`.
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct OciSnapshotConfig {
    /// Hyperlight crate version that produced this config. Recorded
    /// for diagnostics. Not checked on load.
    pub(super) hyperlight_version: String,
    pub(super) arch: Arch,
    /// Memory blob ABI version. See `SNAPSHOT_ABI_VERSION`.
    pub(super) abi_version: u32,
    pub(super) hypervisor: Hypervisor,
    /// CPU vendor captured at snapshot time. Checked on load.
    pub(super) cpu_vendor: CpuVendor,
    /// Top of the guest stack, in guest virtual address space.
    pub(super) stack_top_gva: u64,
    /// Guest virtual address the loader resumes the paused call at.
    pub(super) entrypoint_addr: u64,
    /// Guest virtual address of the ELF entry point
    /// (`load_addr + e_entry - base_va`), preserved across the
    /// Initialise->Call transition. Fills `AT_ENTRY` in core dumps so
    /// gdb resolves PIE symbols.
    pub(super) original_entrypoint_addr: u64,
    /// Special registers captured from the paused vCPU, restored
    /// verbatim when resuming the call.
    pub(super) sregs: CommonSpecialRegisters,
    /// The MSRs saved in this snapshot. An empty field restores the destination
    /// baseline.
    #[cfg(target_arch = "x86_64")]
    pub(super) msrs: Vec<MsrEntry>,
    pub(super) layout: MemoryLayout,
    /// Total size of the memory blob in bytes (including the guest
    /// page-table tail, if any). Equal to `self.memory.mem_size()`.
    pub(super) memory_size: u64,
    /// Names and signatures of host functions registered when this
    /// snapshot was taken. Validated against the loader's registry.
    pub(super) host_functions: Vec<HostFunction>,
    /// Generation counter for the snapshot. Restored verbatim into
    /// the `Snapshot` so guest-visible bookkeeping at
    /// `SCRATCH_TOP_SNAPSHOT_GENERATION_OFFSET` is continuous across
    /// save/load.
    pub(super) snapshot_generation: u64,
    /// Application-owned metadata keyed by namespace.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub(super) metadata: BTreeMap<String, serde_json::Value>,
}

/// Layered OCI config. Manifest descriptors contain data layers in `layers`
/// order, followed by a separate page-table blob and the transport blob.
#[allow(dead_code)]
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct OciSnapshotConfigV4 {
    pub(super) hyperlight_version: String,
    pub(super) arch: Arch,
    pub(super) abi_version: u32,
    pub(super) hypervisor: Hypervisor,
    pub(super) cpu_vendor: CpuVendor,
    pub(super) host_page_size: usize,
    pub(super) stack_top_gva: u64,
    pub(super) entrypoint_addr: u64,
    pub(super) original_entrypoint_addr: u64,
    pub(super) sregs: CommonSpecialRegisters,
    #[cfg(target_arch = "x86_64")]
    pub(super) msrs: Vec<MsrEntry>,
    pub(super) layout: MemoryLayoutV4,
    pub(super) layers: Vec<OciSnapshotLayer>,
    pub(super) page_table_len: usize,
    pub(super) host_functions: Vec<HostFunction>,
    pub(super) snapshot_generation: u64,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub(super) metadata: BTreeMap<String, serde_json::Value>,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct MemoryLayoutV4 {
    pub(super) heap_size: usize,
    pub(super) code_size: usize,
    #[serde(default)]
    pub(super) code_virt_base: u64,
    pub(super) init_data_size: usize,
    pub(super) init_data_permissions: Option<u32>,
    pub(super) scratch_size: usize,
    pub(super) g2h_queue_size: usize,
    pub(super) h2g_queue_size: usize,
    pub(super) g2h_buffer_size: usize,
    pub(super) h2g_buffer_size: usize,
    pub(super) g2h_pool_pages: usize,
    pub(super) h2g_pool_pages: usize,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct OciSnapshotLayer {
    pub(super) data: OciSnapshotDataRange,
    pub(super) live_data: Vec<OciMemoryRange>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct OciSnapshotDataRange {
    pub(super) gpa_start: u64,
    pub(super) len: usize,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct OciMemoryRange {
    pub(super) start: usize,
    pub(super) end: usize,
}

impl From<&std::ops::Range<usize>> for OciMemoryRange {
    fn from(range: &std::ops::Range<usize>) -> Self {
        Self {
            start: range.start,
            end: range.end,
        }
    }
}

impl From<OciMemoryRange> for std::ops::Range<usize> {
    fn from(range: OciMemoryRange) -> Self {
        range.start..range.end
    }
}

impl From<&SnapshotLayer> for OciSnapshotLayer {
    fn from(layer: &SnapshotLayer) -> Self {
        let data = layer.blob().data_gpa_range();
        Self {
            data: OciSnapshotDataRange {
                gpa_start: data.gpa_start(),
                len: data.len(),
            },
            live_data: layer.live_data_ranges().iter().map(Into::into).collect(),
        }
    }
}

impl OciSnapshotLayer {
    fn validate_for_load(
        &self,
        storage_size: usize,
        host_page_size: usize,
        scratch_base: u64,
    ) -> crate::Result<std::ops::Range<u64>> {
        let data = validate_snapshot_blob_layout(
            storage_size,
            self.data.gpa_start,
            self.data.len,
            scratch_base,
            host_page_size,
        )?;
        validate_snapshot_live_data(
            data.len(),
            self.live_data.iter().map(|range| range.start..range.end),
            host_page_size,
        )?;
        Ok(data.gpa_range())
    }
}

#[allow(dead_code)]
impl OciSnapshotConfigV4 {
    /// Sizes correspond to data descriptors; the page-table descriptor follows them.
    pub(super) fn validate_for_load(
        &self,
        data_storage_sizes: &[usize],
        page_table_storage_size: usize,
    ) -> crate::Result<()> {
        if self.arch != Arch::current() {
            return Err(crate::new_error!("snapshot architecture mismatch"));
        }
        if self.abi_version != SNAPSHOT_ABI_VERSION_V4 {
            return Err(crate::new_error!("snapshot ABI version mismatch"));
        }
        let current_hv = Hypervisor::current()
            .ok_or_else(|| crate::new_error!("no hypervisor available to load snapshot"))?;
        if self.hypervisor != current_hv {
            return Err(crate::new_error!("snapshot hypervisor mismatch"));
        }
        if self.cpu_vendor != CpuVendor::current() {
            return Err(crate::new_error!("snapshot CPU vendor mismatch"));
        }
        if self.host_page_size != page_size::get() {
            return Err(crate::new_error!(
                "snapshot host page size mismatch: file uses {}, current host uses {}",
                self.host_page_size,
                page_size::get()
            ));
        }
        self.layout.validate_for_load()?;
        validate_snapshot_layer_count(self.layers.len())?;
        if self.layers.len() != data_storage_sizes.len() {
            return Err(crate::new_error!(
                "snapshot data descriptor count {} does not match config layer count {}",
                data_storage_sizes.len(),
                self.layers.len()
            ));
        }
        validate_snapshot_page_tables(
            page_table_storage_size,
            self.page_table_len,
            self.host_page_size,
        )?;
        let mut retained_bytes = page_table_storage_size;
        let mut mapped_bytes = 0usize;
        let mut mapping_count = 0usize;
        let base_gpa = SandboxMemoryLayout::BASE_ADDRESS as u64;
        let scratch_base = hyperlight_common::layout::scratch_base_gpa(self.layout.scratch_size);
        let mut data_end_gpa = base_gpa;
        let mut data_ranges = Vec::with_capacity(self.layers.len());
        for (layer, &storage_size) in self.layers.iter().zip(data_storage_sizes) {
            let data_range =
                layer.validate_for_load(storage_size, self.host_page_size, scratch_base)?;
            data_end_gpa = data_end_gpa.max(data_range.end);
            data_ranges.push(data_range);
            retained_bytes = retained_bytes
                .checked_add(storage_size)
                .ok_or_else(|| crate::new_error!("snapshot retained byte count overflows"))?;
            mapping_count = mapping_count
                .checked_add(layer.live_data.len())
                .ok_or_else(|| crate::new_error!("snapshot mapping count overflows"))?;
            for range in &layer.live_data {
                mapped_bytes = mapped_bytes
                    .checked_add(range.end - range.start)
                    .ok_or_else(|| crate::new_error!("snapshot mapped byte count overflows"))?;
            }
        }
        validate_snapshot_totals(mapping_count, mapped_bytes, retained_bytes)?;
        data_ranges.sort_unstable_by_key(|range| range.start);
        validate_sorted_snapshot_gpa_ranges(data_ranges)?;
        let address_span = usize::try_from(
            data_end_gpa
                .checked_sub(base_gpa)
                .ok_or_else(|| crate::new_error!("snapshot address span starts below base"))?,
        )?;
        self.validate_address_span(address_span, scratch_base)
    }

    fn validate_address_span(&self, address_span: usize, scratch_base: u64) -> crate::Result<()> {
        if address_span == 0 || !address_span.is_multiple_of(PAGE_SIZE) {
            return Err(crate::new_error!("snapshot GPA span is invalid"));
        }
        let base_gpa = SandboxMemoryLayout::BASE_ADDRESS as u64;
        let code_start = if self.layout.code_virt_base == 0 {
            base_gpa
        } else {
            self.layout.code_virt_base
        };
        let code_end = code_start
            .checked_add(self.layout.code_size.next_multiple_of(PAGE_SIZE) as u64)
            .ok_or_else(|| crate::new_error!("snapshot code region overflows"))?;
        if self.entrypoint_addr < code_start || self.entrypoint_addr >= code_end {
            return Err(crate::new_error!(
                "snapshot entrypoint is outside the code region"
            ));
        }
        #[cfg(target_arch = "aarch64")]
        if !self.entrypoint_addr.is_multiple_of(4) {
            return Err(crate::new_error!(
                "snapshot entrypoint is not 4-byte aligned"
            ));
        }
        let data_end = base_gpa
            .checked_add(address_span as u64)
            .ok_or_else(|| crate::new_error!("snapshot GPA span overflows"))?;
        if data_end > scratch_base {
            return Err(crate::new_error!(
                "snapshot address span overlaps scratch memory"
            ));
        }
        if self.original_entrypoint_addr < code_start || self.original_entrypoint_addr >= code_end {
            return Err(crate::new_error!(
                "snapshot original entrypoint is outside the code region"
            ));
        }
        let max_gva = hyperlight_common::layout::SCRATCH_TOP_GVA as u64;
        if self.stack_top_gva == 0
            || self.stack_top_gva > max_gva
            || !self.stack_top_gva.is_multiple_of(16)
        {
            return Err(crate::new_error!("snapshot stack_top_gva is invalid"));
        }
        Ok(())
    }
}

impl MemoryLayoutV4 {
    fn validate_for_load(&self) -> crate::Result<()> {
        let max_region = SandboxMemoryLayout::MAX_MEMORY_SIZE;
        for (name, value) in [
            ("heap_size", self.heap_size),
            ("code_size", self.code_size),
            ("init_data_size", self.init_data_size),
            ("scratch_size", self.scratch_size),
            ("g2h_buffer_size", self.g2h_buffer_size),
            ("h2g_buffer_size", self.h2g_buffer_size),
            ("g2h_pool_pages", self.g2h_pool_pages),
            ("h2g_pool_pages", self.h2g_pool_pages),
        ] {
            if value > max_region {
                return Err(crate::new_error!(
                    "snapshot layout field {} ({}) exceeds maximum allowed {}",
                    name,
                    value,
                    max_region
                ));
            }
        }
        let mut transport = crate::sandbox::SandboxConfiguration::default();
        transport.set_g2h_queue_size(self.g2h_queue_size);
        transport.set_h2g_queue_size(self.h2g_queue_size);
        transport.set_g2h_buffer_size(self.g2h_buffer_size);
        transport.set_h2g_buffer_size(self.h2g_buffer_size);
        transport.set_g2h_pool_pages(self.g2h_pool_pages);
        transport.set_h2g_pool_pages(self.h2g_pool_pages);
        for (name, saved, normalized) in [
            (
                "g2h_queue_size",
                self.g2h_queue_size,
                transport.get_g2h_queue_size(),
            ),
            (
                "h2g_queue_size",
                self.h2g_queue_size,
                transport.get_h2g_queue_size(),
            ),
            (
                "g2h_buffer_size",
                self.g2h_buffer_size,
                transport.get_g2h_buffer_size(),
            ),
            (
                "h2g_buffer_size",
                self.h2g_buffer_size,
                transport.get_h2g_buffer_size(),
            ),
            (
                "g2h_pool_pages",
                self.g2h_pool_pages,
                transport.get_g2h_pool_pages(),
            ),
            (
                "h2g_pool_pages",
                self.h2g_pool_pages,
                transport.get_h2g_pool_pages(),
            ),
        ] {
            if saved != normalized {
                return Err(crate::new_error!(
                    "snapshot layout field {} ({}) is not a valid transport value",
                    name,
                    saved
                ));
            }
        }
        Ok(())
    }
}

/// Sizes and permissions of the regions inside the snapshot blob,
/// enough for the loader to rebuild a `SandboxMemoryLayout`.
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct MemoryLayout {
    pub(super) heap_size: usize,
    pub(super) code_size: usize,
    /// Virtual base address of the code region. A value of zero means the
    /// code region is identity mapped.
    #[serde(default)]
    pub(super) code_virt_base: u64,
    pub(super) init_data_size: usize,
    /// Memory region flag bits. `None` means default permissions.
    pub(super) init_data_permissions: Option<u32>,
    pub(super) scratch_size: usize,
    pub(super) g2h_queue_size: usize,
    pub(super) h2g_queue_size: usize,
    pub(super) g2h_buffer_size: usize,
    pub(super) h2g_buffer_size: usize,
    pub(super) g2h_pool_pages: usize,
    pub(super) h2g_pool_pages: usize,
    pub(super) snapshot_size: usize,
    pub(super) pt_size: Option<usize>,
}

/// Name and signature of one host function registered when the
/// snapshot was taken. The loader validates these against the
/// registry of the sandbox it is restoring into.
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct HostFunction {
    function_name: String,
    parameter_types: Vec<ParameterTypeRepr>,
    return_type: ReturnTypeRepr,
}

/// JSON-friendly mirror of
/// [`hyperlight_common::flatbuffer_wrappers::function_types::ParameterType`].
/// Kept local so we don't have to plumb serde through `hyperlight_common`.
/// The `match`es below are exhaustive: any new variant upstream forces
/// an explicit decision here.
#[derive(Serialize, Deserialize, Copy, Clone)]
#[serde(rename_all = "snake_case")]
enum ParameterTypeRepr {
    Int,
    UInt,
    Long,
    ULong,
    Float,
    Double,
    String,
    Bool,
    VecBytes,
    ByteChunks,
}

/// JSON-friendly mirror of
/// [`hyperlight_common::flatbuffer_wrappers::function_types::ReturnType`].
#[derive(Serialize, Deserialize, Copy, Clone)]
#[serde(rename_all = "snake_case")]
enum ReturnTypeRepr {
    Int,
    UInt,
    Long,
    ULong,
    Float,
    Double,
    String,
    Bool,
    Void,
    VecBytes,
    ByteChunks,
}

impl From<&ParameterType> for ParameterTypeRepr {
    fn from(p: &ParameterType) -> Self {
        match p {
            ParameterType::Int => Self::Int,
            ParameterType::UInt => Self::UInt,
            ParameterType::Long => Self::Long,
            ParameterType::ULong => Self::ULong,
            ParameterType::Float => Self::Float,
            ParameterType::Double => Self::Double,
            ParameterType::String => Self::String,
            ParameterType::Bool => Self::Bool,
            ParameterType::VecBytes => Self::VecBytes,
            ParameterType::ByteChunks => Self::ByteChunks,
        }
    }
}

impl From<ParameterTypeRepr> for ParameterType {
    fn from(r: ParameterTypeRepr) -> Self {
        match r {
            ParameterTypeRepr::Int => Self::Int,
            ParameterTypeRepr::UInt => Self::UInt,
            ParameterTypeRepr::Long => Self::Long,
            ParameterTypeRepr::ULong => Self::ULong,
            ParameterTypeRepr::Float => Self::Float,
            ParameterTypeRepr::Double => Self::Double,
            ParameterTypeRepr::String => Self::String,
            ParameterTypeRepr::Bool => Self::Bool,
            ParameterTypeRepr::VecBytes => Self::VecBytes,
            ParameterTypeRepr::ByteChunks => Self::ByteChunks,
        }
    }
}

impl From<&ReturnType> for ReturnTypeRepr {
    fn from(r: &ReturnType) -> Self {
        match r {
            ReturnType::Int => Self::Int,
            ReturnType::UInt => Self::UInt,
            ReturnType::Long => Self::Long,
            ReturnType::ULong => Self::ULong,
            ReturnType::Float => Self::Float,
            ReturnType::Double => Self::Double,
            ReturnType::String => Self::String,
            ReturnType::Bool => Self::Bool,
            ReturnType::Void => Self::Void,
            ReturnType::VecBytes => Self::VecBytes,
            ReturnType::ByteChunks => Self::ByteChunks,
        }
    }
}

impl From<ReturnTypeRepr> for ReturnType {
    fn from(r: ReturnTypeRepr) -> Self {
        match r {
            ReturnTypeRepr::Int => Self::Int,
            ReturnTypeRepr::UInt => Self::UInt,
            ReturnTypeRepr::Long => Self::Long,
            ReturnTypeRepr::ULong => Self::ULong,
            ReturnTypeRepr::Float => Self::Float,
            ReturnTypeRepr::Double => Self::Double,
            ReturnTypeRepr::String => Self::String,
            ReturnTypeRepr::Bool => Self::Bool,
            ReturnTypeRepr::Void => Self::Void,
            ReturnTypeRepr::VecBytes => Self::VecBytes,
            ReturnTypeRepr::ByteChunks => Self::ByteChunks,
        }
    }
}

impl From<&HostFunctionDefinition> for HostFunction {
    fn from(d: &HostFunctionDefinition) -> Self {
        let parameter_types = d
            .parameter_types
            .as_ref()
            .map(|v| v.iter().map(ParameterTypeRepr::from).collect())
            .unwrap_or_default();
        Self {
            function_name: d.function_name.clone(),
            parameter_types,
            return_type: ReturnTypeRepr::from(&d.return_type),
        }
    }
}

impl From<HostFunction> for HostFunctionDefinition {
    fn from(r: HostFunction) -> Self {
        Self {
            function_name: r.function_name,
            parameter_types: Some(r.parameter_types.into_iter().map(Into::into).collect()),
            return_type: r.return_type.into(),
        }
    }
}

impl OciSnapshotConfig {
    fn validate_entrypoints(&self, code_lo: u64, code_hi: u64) -> crate::Result<()> {
        if self.entrypoint_addr < code_lo || self.entrypoint_addr >= code_hi {
            return Err(crate::new_error!(
                "snapshot entrypoint addr {:#x} is outside the code region [{:#x}, {:#x})",
                self.entrypoint_addr,
                code_lo,
                code_hi
            ));
        }
        #[cfg(target_arch = "aarch64")]
        if !self.entrypoint_addr.is_multiple_of(4) {
            return Err(crate::new_error!(
                "snapshot entrypoint addr {:#x} is not 4-byte aligned",
                self.entrypoint_addr
            ));
        }
        if self.original_entrypoint_addr < code_lo || self.original_entrypoint_addr >= code_hi {
            return Err(crate::new_error!(
                "snapshot original entrypoint addr {:#x} is outside the code region [{:#x}, {:#x})",
                self.original_entrypoint_addr,
                code_lo,
                code_hi
            ));
        }
        Ok(())
    }

    pub(super) fn validate_for_load(&self) -> crate::Result<()> {
        if self.arch != Arch::current() {
            return Err(crate::new_error!(
                "snapshot architecture mismatch: file is {:?}, current host is {:?} \
                 (snapshot produced by hyperlight {})",
                self.arch,
                Arch::current(),
                self.hyperlight_version
            ));
        }
        if self.abi_version != SNAPSHOT_ABI_VERSION {
            return Err(crate::new_error!(
                "snapshot ABI version mismatch: file has version {}, this build expects {}. \
                 The snapshot must be regenerated from the guest binary \
                 (snapshot produced by hyperlight {}).",
                self.abi_version,
                SNAPSHOT_ABI_VERSION,
                self.hyperlight_version
            ));
        }
        let current_hv = Hypervisor::current()
            .ok_or_else(|| crate::new_error!("no hypervisor available to load snapshot"))?;
        if self.hypervisor != current_hv {
            return Err(crate::new_error!(
                "snapshot hypervisor mismatch: file was created on {} but the current hypervisor is {} \
                 (snapshot produced by hyperlight {})",
                self.hypervisor.name(),
                current_hv.name(),
                self.hyperlight_version
            ));
        }
        let current_vendor = CpuVendor::current();
        if self.cpu_vendor != current_vendor {
            return Err(crate::new_error!(
                "snapshot CPU vendor mismatch: file was created on {} but the current CPU is {} \
                 (snapshot produced by hyperlight {})",
                self.cpu_vendor.as_str(),
                current_vendor.as_str(),
                self.hyperlight_version
            ));
        }
        // Bound memory size early so the subsequent file-size check
        // does not have to deal with absurd values.
        if self.memory_size == 0 || self.memory_size > SandboxMemoryLayout::MAX_MEMORY_SIZE as u64 {
            return Err(crate::new_error!(
                "snapshot memory_size ({}) is out of range",
                self.memory_size
            ));
        }
        if !(self.memory_size as usize).is_multiple_of(PAGE_SIZE) {
            return Err(crate::new_error!(
                "snapshot memory_size ({}) is not a multiple of PAGE_SIZE",
                self.memory_size
            ));
        }
        // `snapshot_size` is the guest-visible prefix of the blob,
        // mapped at `BASE_ADDRESS`. `pt_size` is the page-table tail
        // after it, present in the blob and host mapping but outside
        // the guest mapping. They sum to `memory_size`.
        if self.layout.snapshot_size == 0 {
            return Err(crate::new_error!("snapshot snapshot_size must be nonzero"));
        }
        if !self.layout.snapshot_size.is_multiple_of(PAGE_SIZE) {
            return Err(crate::new_error!(
                "snapshot snapshot_size ({}) is not a multiple of PAGE_SIZE",
                self.layout.snapshot_size
            ));
        }
        let pt = self
            .layout
            .pt_size
            .ok_or_else(|| crate::new_error!("snapshot pt_size is missing"))?;
        if pt == 0 {
            return Err(crate::new_error!("snapshot pt_size must be nonzero"));
        }
        if !pt.is_multiple_of(PAGE_SIZE) {
            return Err(crate::new_error!(
                "snapshot pt_size ({}) is not a multiple of PAGE_SIZE",
                pt
            ));
        }
        // The total memory size might be bigger because it has to
        // take into account the host page size, as well as the guest
        // page size.
        let total_size = (self.layout.snapshot_size as u64)
            .saturating_add(pt as u64)
            .next_multiple_of(page_size::get() as u64);
        if total_size != self.memory_size {
            return Err(crate::new_error!(
                "snapshot snapshot_size ({}) + pt_size ({}), rounded to {}, does not equal memory_size ({})",
                self.layout.snapshot_size,
                pt,
                total_size,
                self.memory_size
            ));
        }
        // Cap each layout field at `MAX_MEMORY_SIZE` so the later
        // size and offset sums in `SandboxMemoryLayout` cannot
        // overflow `u64`. Whether the regions fit the snapshot is
        // checked against `snapshot_size` in `load_inner`.
        let max_region = SandboxMemoryLayout::MAX_MEMORY_SIZE;
        for (name, value) in [
            ("heap_size", self.layout.heap_size),
            ("code_size", self.layout.code_size),
            ("init_data_size", self.layout.init_data_size),
            ("scratch_size", self.layout.scratch_size),
            ("g2h_buffer_size", self.layout.g2h_buffer_size),
            ("h2g_buffer_size", self.layout.h2g_buffer_size),
            ("g2h_pool_pages", self.layout.g2h_pool_pages),
            ("h2g_pool_pages", self.layout.h2g_pool_pages),
        ] {
            if value > max_region {
                return Err(crate::new_error!(
                    "snapshot layout field {} ({}) exceeds maximum allowed {}",
                    name,
                    value,
                    max_region
                ));
            }
        }

        let mut transport = crate::sandbox::SandboxConfiguration::default();
        transport.set_g2h_queue_size(self.layout.g2h_queue_size);
        transport.set_h2g_queue_size(self.layout.h2g_queue_size);
        transport.set_g2h_buffer_size(self.layout.g2h_buffer_size);
        transport.set_h2g_buffer_size(self.layout.h2g_buffer_size);
        transport.set_g2h_pool_pages(self.layout.g2h_pool_pages);
        transport.set_h2g_pool_pages(self.layout.h2g_pool_pages);

        for (name, saved, normalized) in [
            (
                "g2h_queue_size",
                self.layout.g2h_queue_size,
                transport.get_g2h_queue_size(),
            ),
            (
                "h2g_queue_size",
                self.layout.h2g_queue_size,
                transport.get_h2g_queue_size(),
            ),
            (
                "g2h_buffer_size",
                self.layout.g2h_buffer_size,
                transport.get_g2h_buffer_size(),
            ),
            (
                "h2g_buffer_size",
                self.layout.h2g_buffer_size,
                transport.get_h2g_buffer_size(),
            ),
            (
                "g2h_pool_pages",
                self.layout.g2h_pool_pages,
                transport.get_g2h_pool_pages(),
            ),
            (
                "h2g_pool_pages",
                self.layout.h2g_pool_pages,
                transport.get_h2g_pool_pages(),
            ),
        ] {
            if saved != normalized {
                return Err(crate::new_error!(
                    "snapshot layout field {} ({}) is not a valid transport value",
                    name,
                    saved
                ));
            }
        }

        // The saved dispatch entrypoint must be in the executable code
        // region. For non-PIE or ASLR guests the code region's virtual
        // base differs from the physical load address.
        let code_lo = if self.layout.code_virt_base != 0 {
            self.layout.code_virt_base
        } else {
            SandboxMemoryLayout::BASE_ADDRESS as u64
        };
        let code_hi = code_lo
            .checked_add(self.layout.code_size.next_multiple_of(PAGE_SIZE) as u64)
            .ok_or_else(|| {
                crate::new_error!(
                    "snapshot layout overflow: code_virt_base ({:#x}) + code_size ({}) does not fit in u64",
                    code_lo,
                    self.layout.code_size
                )
            })?;
        self.validate_entrypoints(code_lo, code_hi)?;

        // `stack_top_gva` is restored directly into the guest stack
        // pointer. It must be aligned and in the guest address range.
        let max_gva = hyperlight_common::layout::SCRATCH_TOP_GVA as u64;
        if self.stack_top_gva == 0 || self.stack_top_gva > max_gva {
            return Err(crate::new_error!(
                "snapshot stack_top_gva {:#x} is outside the valid range (0, {:#x}]",
                self.stack_top_gva,
                max_gva
            ));
        }
        if !self.stack_top_gva.is_multiple_of(16) {
            return Err(crate::new_error!(
                "snapshot stack_top_gva {:#x} is not 16-byte aligned",
                self.stack_top_gva
            ));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use hyperlight_common::flatbuffer_wrappers::function_types::{ParameterType, ReturnType};

    use super::*;
    #[cfg(target_arch = "x86_64")]
    use crate::hypervisor::regs::{CommonSegmentRegister, CommonTableRegister};

    /// Build a `CommonSegmentRegister` whose every field holds a
    /// distinct value, so a transposed field in the
    /// `CommonSpecialRegisters` conversion produces an inequality.
    #[cfg(target_arch = "x86_64")]
    fn distinct_segment(start: u64) -> CommonSegmentRegister {
        CommonSegmentRegister {
            base: start,
            limit: (start + 1) as u32,
            selector: (start + 2) as u16,
            type_: (start + 3) as u8,
            present: (start + 4) as u8,
            dpl: (start + 5) as u8,
            db: (start + 6) as u8,
            s: (start + 7) as u8,
            l: (start + 8) as u8,
            g: (start + 9) as u8,
            avl: (start + 10) as u8,
            unusable: (start + 11) as u8,
            padding: (start + 12) as u8,
        }
    }

    #[cfg(target_arch = "x86_64")]
    fn distinct_table(start: u64) -> CommonTableRegister {
        CommonTableRegister {
            base: start,
            limit: (start + 1) as u16,
        }
    }

    /// Special registers with a unique value in every field, including
    /// a nonzero `cr3`.
    fn distinct_sregs() -> CommonSpecialRegisters {
        #[cfg(target_arch = "x86_64")]
        let sr = CommonSpecialRegisters {
            cs: distinct_segment(10),
            ds: distinct_segment(30),
            es: distinct_segment(50),
            fs: distinct_segment(70),
            gs: distinct_segment(90),
            ss: distinct_segment(110),
            tr: distinct_segment(130),
            ldt: distinct_segment(150),
            gdt: distinct_table(170),
            idt: distinct_table(180),
            cr0: 200,
            cr2: 201,
            cr3: 202,
            cr4: 203,
            cr8: 204,
            efer: 205,
            apic_base: 206,
            interrupt_bitmap: [207, 208, 209, 210],
        };
        #[cfg(target_arch = "aarch64")]
        let sr = CommonSpecialRegisters {
            ttbr0_el1: 10,
            tcr_el1: 20,
            mair_el1: 30,
            sctlr_el1: 40,
            cpacr_el1: 50,
            vbar_el1: 60,
            sp_el1: 60,
        };
        sr
    }

    /// Round-tripping special registers through serde preserves every
    /// field. `cr3` is the sole exception: it is omitted from the
    /// config and recomputed at load, so it returns as zero.
    #[test]
    fn sregs_round_trip_preserves_all_fields_except_cr3() {
        let original = distinct_sregs();
        let serialized = serde_json::to_vec(&original).unwrap();
        let restored: CommonSpecialRegisters = serde_json::from_slice(&serialized).unwrap();

        let mut expected = original;
        #[cfg(target_arch = "x86_64")]
        {
            expected.cr3 = 0;
        }
        #[cfg(target_arch = "aarch64")]
        {
            expected.ttbr0_el1 = 0;
        }
        assert_eq!(restored, expected);
    }

    /// Captured MSRs survive the serde round-trip through the config,
    /// including index and value for every entry.
    #[cfg(target_arch = "x86_64")]
    #[test]
    fn msrs_round_trip_preserves_every_entry() {
        let original = gating_config_with_msrs(Some(vec![
            MsrEntry {
                index: 0xC000_0102,
                value: 0xDEAD_BEEF,
            },
            MsrEntry {
                index: 0x10,
                value: 0x1234_5678_9ABC_DEF0,
            },
        ]));
        let json = serde_json::to_vec(&original).unwrap();
        let restored: OciSnapshotConfig = serde_json::from_slice(&json).unwrap();
        assert_eq!(restored.msrs, original.msrs);
    }

    /// A config JSON with no MSR state is rejected.
    #[cfg(target_arch = "x86_64")]
    #[test]
    fn config_without_msrs_is_rejected() {
        let with = gating_config_with_msrs(Some(vec![MsrEntry {
            index: 0x10,
            value: 1,
        }]));
        let mut json: serde_json::Value =
            serde_json::from_slice(&serde_json::to_vec(&with).unwrap()).unwrap();
        assert!(json.as_object_mut().unwrap().remove("msrs").is_some());

        let err = serde_json::from_value::<OciSnapshotConfig>(json)
            .err()
            .expect("config without msrs should fail to deserialize")
            .to_string();
        assert!(err.contains("missing field `msrs`"), "got: {err}");
    }

    /// Every `ParameterType` survives the round-trip through its serde
    /// mirror, guarding against a transposed variant in either match.
    #[test]
    fn parameter_type_repr_round_trips_every_variant() {
        let variants = [
            ParameterType::Int,
            ParameterType::UInt,
            ParameterType::Long,
            ParameterType::ULong,
            ParameterType::Float,
            ParameterType::Double,
            ParameterType::String,
            ParameterType::Bool,
            ParameterType::VecBytes,
            ParameterType::ByteChunks,
        ];
        for p in variants {
            let back: ParameterType = ParameterTypeRepr::from(&p).into();
            assert_eq!(back, p, "parameter type {:?} did not round-trip", p);
        }
    }

    /// Every `ReturnType` survives the round-trip through its serde
    /// mirror, guarding against a transposed variant in either match.
    #[test]
    fn return_type_repr_round_trips_every_variant() {
        let variants = [
            ReturnType::Int,
            ReturnType::UInt,
            ReturnType::Long,
            ReturnType::ULong,
            ReturnType::Float,
            ReturnType::Double,
            ReturnType::String,
            ReturnType::Bool,
            ReturnType::Void,
            ReturnType::VecBytes,
            ReturnType::ByteChunks,
        ];
        for r in variants {
            let back: ReturnType = ReturnTypeRepr::from(&r).into();
            assert_eq!(back, r, "return type {:?} did not round-trip", r);
        }
    }

    /// `CpuVendor::current` returns the expected host vendor. Ignored
    /// by default and run explicitly in CI, where the runner hardware
    /// is known. Extend the allowlist when new runner hardware is
    /// added.
    #[test]
    #[ignore = "hardware-specific; run explicitly in CI"]
    fn cpu_vendor_current_is_recognized() {
        let vendor = CpuVendor::current();
        let v = vendor.as_str();
        #[cfg(target_arch = "x86_64")]
        assert!(
            matches!(v, "GenuineIntel" | "AuthenticAMD"),
            "unrecognized x86_64 CPU vendor: {v:?}"
        );
        #[cfg(all(target_arch = "aarch64", target_os = "linux"))]
        // MIDR_EL1 implementer byte for Apple silicon.
        assert_eq!(v, "0x61", "unexpected aarch64 CPU implementer");
        #[cfg(all(target_arch = "aarch64", target_os = "macos"))]
        assert_eq!(v, "0x61", "unexpected aarch64 CPU implementer");
    }

    /// The architecture the current host is not running.
    fn other_arch() -> Arch {
        match Arch::current() {
            Arch::X86_64 => Arch::Aarch64,
            Arch::Aarch64 => Arch::X86_64,
        }
    }

    /// A config whose `arch` and `abi_version` match the current
    /// build, so the architecture and ABI gates pass and a test can
    /// trip a later gate in isolation. The layout is minimal: the
    /// gating checks under test short-circuit before reading it.
    fn gating_config() -> OciSnapshotConfig {
        OciSnapshotConfig {
            hyperlight_version: "test".to_string(),
            arch: Arch::current(),
            abi_version: SNAPSHOT_ABI_VERSION,
            hypervisor: Hypervisor::Mshv,
            cpu_vendor: CpuVendor::current(),
            stack_top_gva: 0x2000,
            entrypoint_addr: SandboxMemoryLayout::BASE_ADDRESS as u64,
            original_entrypoint_addr: SandboxMemoryLayout::BASE_ADDRESS as u64,
            sregs: distinct_sregs(),
            #[cfg(target_arch = "x86_64")]
            msrs: Vec::new(),
            layout: MemoryLayout {
                heap_size: 0,
                code_size: 0,
                code_virt_base: 0,
                init_data_size: 0,
                init_data_permissions: None,
                scratch_size: 0,
                g2h_queue_size: 64,
                h2g_queue_size: 32,
                g2h_buffer_size: PAGE_SIZE,
                h2g_buffer_size: PAGE_SIZE,
                g2h_pool_pages: 8,
                h2g_pool_pages: 4,
                snapshot_size: PAGE_SIZE,
                pt_size: None,
            },
            memory_size: PAGE_SIZE as u64,
            host_functions: Vec::new(),
            snapshot_generation: 0,
            metadata: BTreeMap::new(),
        }
    }

    /// `gating_config` with a chosen MSR set, for serde tests.
    #[cfg(target_arch = "x86_64")]
    fn gating_config_with_msrs(msrs: Option<Vec<MsrEntry>>) -> OciSnapshotConfig {
        OciSnapshotConfig {
            msrs: msrs.unwrap_or_default(),
            ..gating_config()
        }
    }

    /// A snapshot built for a different architecture is rejected.
    #[test]
    fn validate_for_load_rejects_arch_mismatch() {
        let mut cfg = gating_config();
        cfg.arch = other_arch();
        let err = cfg.validate_for_load().unwrap_err().to_string();
        assert!(err.contains("architecture mismatch"), "got: {err}");
    }

    /// A snapshot stamped with a different ABI version is rejected.
    #[test]
    fn validate_for_load_rejects_abi_version_mismatch() {
        let mut cfg = gating_config();
        cfg.abi_version = SNAPSHOT_ABI_VERSION.wrapping_add(1);
        let err = cfg.validate_for_load().unwrap_err().to_string();
        assert!(err.contains("ABI version mismatch"), "got: {err}");
    }

    #[test]
    fn validate_entrypoints_rejects_original_entrypoint_after_code() {
        let mut cfg = gating_config();
        let code_lo = SandboxMemoryLayout::BASE_ADDRESS as u64;
        let code_hi = code_lo + PAGE_SIZE as u64;
        cfg.entrypoint_addr = code_lo;
        cfg.original_entrypoint_addr = code_hi;

        let err = cfg
            .validate_entrypoints(code_lo, code_hi)
            .unwrap_err()
            .to_string();
        assert!(err.contains("original entrypoint"), "got: {err}");
    }

    /// A snapshot captured under a different hypervisor backend is
    /// rejected. Without a live backend the load is rejected outright,
    /// which exercises the same gate from the other side.
    #[test]
    fn validate_for_load_rejects_hypervisor_mismatch() {
        let Some(current) = Hypervisor::current() else {
            let cfg = gating_config();
            let err = cfg.validate_for_load().unwrap_err().to_string();
            assert!(err.contains("no hypervisor available"), "got: {err}");
            return;
        };
        let other = [Hypervisor::Kvm, Hypervisor::Mshv, Hypervisor::Whp]
            .into_iter()
            .find(|h| *h != current)
            .expect("three backends, at least one differs from current");
        let mut cfg = gating_config();
        cfg.hypervisor = other;
        let err = cfg.validate_for_load().unwrap_err().to_string();
        assert!(err.contains("hypervisor mismatch"), "got: {err}");
    }

    fn layered_config() -> OciSnapshotConfigV4 {
        let current = gating_config();
        let host_page_size = page_size::get();
        OciSnapshotConfigV4 {
            hyperlight_version: current.hyperlight_version,
            arch: current.arch,
            abi_version: SNAPSHOT_ABI_VERSION_V4,
            hypervisor: current.hypervisor,
            cpu_vendor: current.cpu_vendor,
            host_page_size,
            stack_top_gva: current.stack_top_gva,
            entrypoint_addr: current.entrypoint_addr,
            original_entrypoint_addr: current.original_entrypoint_addr,
            sregs: current.sregs,
            #[cfg(target_arch = "x86_64")]
            msrs: current.msrs,
            layout: MemoryLayoutV4 {
                heap_size: current.layout.heap_size,
                code_size: host_page_size,
                code_virt_base: current.layout.code_virt_base,
                init_data_size: current.layout.init_data_size,
                init_data_permissions: current.layout.init_data_permissions,
                scratch_size: current.layout.scratch_size,
                g2h_queue_size: current.layout.g2h_queue_size,
                h2g_queue_size: current.layout.h2g_queue_size,
                g2h_buffer_size: current.layout.g2h_buffer_size,
                h2g_buffer_size: current.layout.h2g_buffer_size,
                g2h_pool_pages: current.layout.g2h_pool_pages,
                h2g_pool_pages: current.layout.h2g_pool_pages,
            },
            layers: vec![OciSnapshotLayer {
                data: OciSnapshotDataRange {
                    gpa_start: SandboxMemoryLayout::BASE_ADDRESS as u64,
                    len: host_page_size,
                },
                live_data: vec![OciMemoryRange {
                    start: 0,
                    end: host_page_size,
                }],
            }],
            page_table_len: PAGE_SIZE,
            host_functions: current.host_functions,
            snapshot_generation: current.snapshot_generation,
            metadata: current.metadata,
        }
    }

    #[test]
    fn layered_config_json_schema() {
        let config = layered_config();
        let value = serde_json::to_value(&config).unwrap();
        assert_eq!(value["abi_version"], SNAPSHOT_ABI_VERSION_V4);
        assert_eq!(value["host_page_size"], page_size::get());
        assert_eq!(
            value["layers"][0]["data"]["gpa_start"],
            config.layers[0].data.gpa_start
        );
        assert_eq!(value["layers"][0]["live_data"][0]["end"], page_size::get());
        assert_eq!(value["page_table_len"], PAGE_SIZE);
        assert!(value.get("memory_size").is_none());
        assert!(value["layout"].get("snapshot_size").is_none());
        assert!(value["layout"].get("pt_size").is_none());
        let restored: OciSnapshotConfigV4 = serde_json::from_value(value.clone()).unwrap();
        assert_eq!(serde_json::to_value(restored).unwrap(), value);

        let mut extra = value.clone();
        extra["layers"][0]["data"]["pt_size"] = serde_json::json!(PAGE_SIZE);
        assert!(serde_json::from_value::<OciSnapshotConfigV4>(extra).is_err());
        let mut missing = value;
        missing.as_object_mut().unwrap().remove("page_table_len");
        assert!(serde_json::from_value::<OciSnapshotConfigV4>(missing).is_err());
    }

    #[test]
    fn layered_config_preserves_metadata_and_code_base() {
        let mut config = layered_config();
        config.layout.code_virt_base = 0x400000;
        config
            .metadata
            .insert("runtime".to_string(), serde_json::json!({"version": 1}));
        let mut value = serde_json::to_value(&config).unwrap();
        let restored: OciSnapshotConfigV4 = serde_json::from_value(value.clone()).unwrap();
        assert_eq!(restored.layout.code_virt_base, config.layout.code_virt_base);
        assert_eq!(restored.metadata, config.metadata);

        value.as_object_mut().unwrap().remove("metadata");
        value["layout"]
            .as_object_mut()
            .unwrap()
            .remove("code_virt_base");
        let restored: OciSnapshotConfigV4 = serde_json::from_value(value).unwrap();
        assert!(restored.metadata.is_empty());
        assert_eq!(restored.layout.code_virt_base, 0);
    }

    #[test]
    fn layered_config_validates_relocated_entrypoints() {
        let mut config = layered_config();
        config.layout.code_virt_base = 0x400000;
        config.entrypoint_addr = config.layout.code_virt_base;
        config.original_entrypoint_addr = config.layout.code_virt_base;
        let scratch_base = hyperlight_common::layout::scratch_base_gpa(config.layout.scratch_size);
        config
            .validate_address_span(page_size::get(), scratch_base)
            .unwrap();

        config.entrypoint_addr += config.layout.code_size as u64;
        assert!(
            config
                .validate_address_span(page_size::get(), scratch_base)
                .is_err()
        );
        config.entrypoint_addr = config.layout.code_virt_base;
        config.original_entrypoint_addr = SandboxMemoryLayout::BASE_ADDRESS as u64;
        assert!(
            config
                .validate_address_span(page_size::get(), scratch_base)
                .is_err()
        );
    }

    #[test]
    fn layered_config_validates_separate_page_tables_and_transport() {
        let Some(hypervisor) = Hypervisor::current() else {
            return;
        };
        let mut config = layered_config();
        config.hypervisor = hypervisor;
        let host_page_size = page_size::get();
        config
            .validate_for_load(&[host_page_size], host_page_size)
            .unwrap();

        assert!(config.validate_for_load(&[], host_page_size).is_err());
        assert!(config.validate_for_load(&[0], host_page_size).is_err());
        assert!(config.validate_for_load(&[host_page_size], 0).is_err());

        config.layers[0].live_data[0].end = 0;
        assert!(
            config
                .validate_for_load(&[host_page_size], host_page_size)
                .is_err()
        );
        config.layers[0].live_data[0].end = host_page_size;
        config.layout.g2h_queue_size = 0;
        assert!(
            config
                .validate_for_load(&[host_page_size], host_page_size)
                .is_err()
        );
    }

    #[test]
    fn layered_config_rejects_overlapping_data_layers() {
        let Some(hypervisor) = Hypervisor::current() else {
            return;
        };
        let mut config = layered_config();
        config.hypervisor = hypervisor;
        let host_page_size = page_size::get();
        config.layers.push(config.layers[0].clone());
        config.layers[1].data.gpa_start += host_page_size as u64;
        config
            .validate_for_load(&[host_page_size, host_page_size], host_page_size)
            .unwrap();
        config.layers[1].data.gpa_start = config.layers[0].data.gpa_start;
        let err = config
            .validate_for_load(&[host_page_size, host_page_size], host_page_size)
            .unwrap_err()
            .to_string();
        assert!(err.contains("overlap"), "got: {err}");
    }
}

#[cfg(test)]
mod schema_pin {
    use super::*;

    #[cfg(target_arch = "x86_64")]
    const PINNED_CALL: &str = r#"{
  "hyperlight_version": "x.y.z",
  "arch": "x86_64",
  "abi_version": 5,
  "hypervisor": "mshv",
  "cpu_vendor": "intel",
  "stack_top_gva": 3735928559,
  "entrypoint_addr": 8192,
  "original_entrypoint_addr": 4096,
  "sregs": {
    "cs": {
      "base": 1,
      "limit": 2,
      "selector": 3,
      "type_": 4,
      "present": 5,
      "dpl": 6,
      "db": 7,
      "s": 8,
      "l": 9,
      "g": 10,
      "avl": 11,
      "unusable": 12,
      "padding": 13
    },
    "ds": {
      "base": 1,
      "limit": 2,
      "selector": 3,
      "type_": 4,
      "present": 5,
      "dpl": 6,
      "db": 7,
      "s": 8,
      "l": 9,
      "g": 10,
      "avl": 11,
      "unusable": 12,
      "padding": 13
    },
    "es": {
      "base": 1,
      "limit": 2,
      "selector": 3,
      "type_": 4,
      "present": 5,
      "dpl": 6,
      "db": 7,
      "s": 8,
      "l": 9,
      "g": 10,
      "avl": 11,
      "unusable": 12,
      "padding": 13
    },
    "fs": {
      "base": 1,
      "limit": 2,
      "selector": 3,
      "type_": 4,
      "present": 5,
      "dpl": 6,
      "db": 7,
      "s": 8,
      "l": 9,
      "g": 10,
      "avl": 11,
      "unusable": 12,
      "padding": 13
    },
    "gs": {
      "base": 1,
      "limit": 2,
      "selector": 3,
      "type_": 4,
      "present": 5,
      "dpl": 6,
      "db": 7,
      "s": 8,
      "l": 9,
      "g": 10,
      "avl": 11,
      "unusable": 12,
      "padding": 13
    },
    "ss": {
      "base": 1,
      "limit": 2,
      "selector": 3,
      "type_": 4,
      "present": 5,
      "dpl": 6,
      "db": 7,
      "s": 8,
      "l": 9,
      "g": 10,
      "avl": 11,
      "unusable": 12,
      "padding": 13
    },
    "tr": {
      "base": 1,
      "limit": 2,
      "selector": 3,
      "type_": 4,
      "present": 5,
      "dpl": 6,
      "db": 7,
      "s": 8,
      "l": 9,
      "g": 10,
      "avl": 11,
      "unusable": 12,
      "padding": 13
    },
    "ldt": {
      "base": 1,
      "limit": 2,
      "selector": 3,
      "type_": 4,
      "present": 5,
      "dpl": 6,
      "db": 7,
      "s": 8,
      "l": 9,
      "g": 10,
      "avl": 11,
      "unusable": 12,
      "padding": 13
    },
    "gdt": {
      "base": 1,
      "limit": 2
    },
    "idt": {
      "base": 3,
      "limit": 4
    },
    "cr0": 1,
    "cr2": 2,
    "cr4": 4,
    "cr8": 5,
    "efer": 6,
    "apic_base": 7,
    "interrupt_bitmap": [
      8,
      9,
      10,
      11
    ]
  },
  "msrs": [
    {
      "index": 16,
      "value": 42
    },
    {
      "index": 3221225474,
      "value": 3735928559
    }
  ],
  "layout": {
    "heap_size": 3,
    "code_size": 4,
    "code_virt_base": 0,
    "init_data_size": 5,
    "init_data_permissions": null,
    "scratch_size": 8,
    "g2h_queue_size": 64,
    "h2g_queue_size": 32,
    "g2h_buffer_size": 4096,
    "h2g_buffer_size": 4096,
    "g2h_pool_pages": 8,
    "h2g_pool_pages": 4,
    "snapshot_size": 9,
    "pt_size": null
  },
  "memory_size": 65536,
  "host_functions": [
    {
      "function_name": "fn_void",
      "parameter_types": [
        "bool"
      ],
      "return_type": "void"
    }
  ],
  "snapshot_generation": 42
}"#;

    #[cfg(target_arch = "aarch64")]
    const PINNED_CALL: &str = r#"{
  "hyperlight_version": "x.y.z",
  "arch": "aarch64",
  "abi_version": 5,
  "hypervisor": "mshv",
  "cpu_vendor": "intel",
  "stack_top_gva": 3735928559,
  "entrypoint_addr": 8192,
  "original_entrypoint_addr": 4096,
  "sregs": {
    "tcr_el1": 1,
    "mair_el1": 2,
    "sctlr_el1": 3,
    "cpacr_el1": 4,
    "vbar_el1": 5,
    "sp_el1": 6
  },
  "layout": {
    "heap_size": 3,
    "code_size": 4,
    "code_virt_base": 0,
    "init_data_size": 5,
    "init_data_permissions": null,
    "scratch_size": 8,
    "g2h_queue_size": 64,
    "h2g_queue_size": 32,
    "g2h_buffer_size": 4096,
    "h2g_buffer_size": 4096,
    "g2h_pool_pages": 8,
    "h2g_pool_pages": 4,
    "snapshot_size": 9,
    "pt_size": null
  },
  "memory_size": 65536,
  "host_functions": [
    {
      "function_name": "fn_void",
      "parameter_types": [
        "bool"
      ],
      "return_type": "void"
    }
  ],
  "snapshot_generation": 42
}"#;

    const PINNED_ARCH: &str = r#"[
  "x86_64",
  "aarch64"
]"#;

    const PINNED_HYPERVISOR: &str = r#"[
  "kvm",
  "mshv",
  "whp"
]"#;

    fn assert_round_trip(pinned: &str) {
        let pinned_value: serde_json::Value =
            serde_json::from_str(pinned).expect("pinned JSON must deserialize as a value");
        let parsed: OciSnapshotConfig =
            serde_json::from_value(pinned_value.clone()).expect("pinned JSON must deserialize");
        let actual = serde_json::to_string_pretty(&parsed).expect("serialize");
        let actual_value: serde_json::Value =
            serde_json::from_str(&actual).expect("serialized config must deserialize");
        assert_eq!(
            actual_value, pinned_value,
            "Snapshot config JSON schema changed. If the change can break \
             existing snapshots on disk, bump `MT_CONFIG_CURRENT` in \
             `super::media_types` and follow `docs/snapshot-versioning.md`. \
             Either way, paste the actual output below into the matching \
             `PINNED_*`.\n\nactual:\n{actual}"
        );
    }

    #[test]
    fn call_round_trip() {
        assert_round_trip(PINNED_CALL);
    }

    #[test]
    fn arch_variants_round_trip() {
        let parsed: Vec<Arch> =
            serde_json::from_str(PINNED_ARCH).expect("pinned arch JSON must deserialize");
        let actual = serde_json::to_string_pretty(&parsed).expect("serialize");
        assert_eq!(actual.trim(), PINNED_ARCH.trim(), "Arch variants changed.");
    }

    #[test]
    fn hypervisor_variants_round_trip() {
        let parsed: Vec<Hypervisor> = serde_json::from_str(PINNED_HYPERVISOR)
            .expect("pinned hypervisor JSON must deserialize");
        let actual = serde_json::to_string_pretty(&parsed).expect("serialize");
        assert_eq!(
            actual.trim(),
            PINNED_HYPERVISOR.trim(),
            "Hypervisor variants changed."
        );
    }
}
