// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 The Hyperlight Authors.

use hyperlight_common::flatbuffer_wrappers::function_types::{ParameterType, ReturnType};
use hyperlight_common::flatbuffer_wrappers::host_function_definition::HostFunctionDefinition;
use hyperlight_common::vmem::PAGE_SIZE;
use serde::{Deserialize, Serialize};

use super::media_types::SNAPSHOT_ABI_VERSION;
use crate::hypervisor::regs::CommonSpecialRegisters;
#[cfg(target_arch = "x86_64")]
use crate::hypervisor::regs::MsrEntry;
use crate::mem::layout::SandboxMemoryLayout;
use crate::sandbox::snapshot::SnapshotLayer;
use crate::sandbox::snapshot::snapshot_memory::{
    validate_snapshot_blob_layout, validate_snapshot_layer_count, validate_snapshot_live_data,
    validate_snapshot_totals, validate_sorted_snapshot_gpa_ranges,
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

/// Current OCI config. Manifest layer `n` stores the immutable blob
/// described by `layers[n]`.
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct OciSnapshotConfig {
    /// Hyperlight crate version that produced this config. Recorded
    /// for diagnostics. Not checked on load.
    pub(super) hyperlight_version: String,
    pub(super) arch: Arch,
    /// Snapshot ABI version. See `SNAPSHOT_ABI_VERSION`.
    pub(super) abi_version: u32,
    pub(super) hypervisor: Hypervisor,
    /// CPU vendor captured at snapshot time. Checked on load.
    pub(super) cpu_vendor: CpuVendor,
    /// Host page size used to align layer storage. Checked on load.
    pub(super) host_page_size: usize,
    /// Top of the guest stack, in guest virtual address space.
    pub(super) stack_top_gva: u64,
    /// Guest virtual address where the loader resumes the paused call.
    pub(super) entrypoint_addr: u64,
    /// Guest virtual address of the ELF entry point. Fills `AT_ENTRY`
    /// in core dumps so gdb resolves PIE symbols.
    pub(super) original_entrypoint_addr: u64,
    /// Special registers restored when the paused call resumes.
    pub(super) sregs: CommonSpecialRegisters,
    /// Saved MSRs. An empty list restores the destination baseline.
    #[cfg(target_arch = "x86_64")]
    pub(super) msrs: Vec<MsrEntry>,
    pub(super) layout: MemoryLayout,
    /// Snapshot layers in manifest order.
    pub(super) layers: Vec<OciSnapshotLayer>,
    /// Index of the layer whose page tables are restored.
    pub(super) active_page_table_layer: usize,
    /// Host functions required by the restored sandbox.
    pub(super) host_functions: Vec<HostFunction>,
    /// Generation restored into guest-visible snapshot bookkeeping.
    pub(super) snapshot_generation: u64,
}

/// Metadata for one immutable snapshot blob.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct OciSnapshotLayer {
    /// Guest-addressable blob prefix.
    pub(super) data: Option<OciSnapshotDataRange>,
    /// Data ranges that remain live after newer layers are applied.
    /// Offsets are relative to the blob's data prefix.
    pub(super) live_data: Vec<OciMemoryRange>,
    /// Host-only page-table range within the blob.
    pub(super) page_tables: Option<OciMemoryRange>,
}

/// Location and length of a blob's guest-addressable prefix.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct OciSnapshotDataRange {
    /// GPA corresponding to blob offset zero.
    pub(super) gpa_start: u64,
    pub(super) len: usize,
}

/// Half-open byte range within a snapshot blob.
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
        let blob = layer.blob();
        Self {
            data: blob.data().map(|data| OciSnapshotDataRange {
                gpa_start: data.gpa_start(),
                len: data.len(),
            }),
            live_data: layer.live_data().iter().map(Into::into).collect(),
            page_tables: blob.page_tables().map(Into::into),
        }
    }
}

impl OciSnapshotLayer {
    fn validate_for_load(
        &self,
        storage_size: usize,
        host_page_size: usize,
        scratch_base: u64,
    ) -> crate::Result<Option<std::ops::Range<u64>>> {
        let data_len = self.data.as_ref().map_or(0, |data| data.len);
        let page_tables = self
            .page_tables
            .as_ref()
            .map(|range| range.start..range.end);
        let data = validate_snapshot_blob_layout(
            storage_size,
            self.data.as_ref().map(|data| data.gpa_start),
            data_len,
            page_tables.as_ref(),
            scratch_base,
            host_page_size,
        )?;
        validate_snapshot_live_data(
            data.as_ref().map(|data| data.len()),
            self.live_data.iter().map(|range| range.start..range.end),
            host_page_size,
        )?;
        Ok(data.map(|data| data.gpa_range()))
    }
}

impl OciSnapshotConfig {
    pub(super) fn validate_for_load(&self, layer_storage_sizes: &[usize]) -> crate::Result<()> {
        validate_platform(
            &self.hyperlight_version,
            self.arch,
            self.abi_version,
            self.hypervisor,
            &self.cpu_vendor,
        )?;
        let current_page_size = page_size::get();
        if self.host_page_size != current_page_size {
            return Err(crate::new_error!(
                "snapshot host page size mismatch: file uses {}, current host uses {}",
                self.host_page_size,
                current_page_size
            ));
        }
        validate_memory_layout(&self.layout)?;
        validate_snapshot_layer_count(self.layers.len())?;
        if self.layers.len() != layer_storage_sizes.len() {
            return Err(crate::new_error!(
                "OCI layer count {} does not match config layer count {}",
                layer_storage_sizes.len(),
                self.layers.len()
            ));
        }
        let active_layer = self
            .layers
            .get(self.active_page_table_layer)
            .ok_or_else(|| {
                crate::new_error!(
                    "active snapshot page-table layer {} is out of bounds",
                    self.active_page_table_layer
                )
            })?;
        let mut retained_bytes = 0usize;
        let mut mapped_bytes = 0usize;
        let mut extent_count = 0usize;
        let base_gpa = SandboxMemoryLayout::BASE_ADDRESS as u64;
        let scratch_base = hyperlight_common::layout::scratch_base_gpa(self.layout.scratch_size);
        let mut data_end_gpa = base_gpa;
        let mut data_ranges = Vec::with_capacity(self.layers.len());
        for (layer, &storage_size) in self.layers.iter().zip(layer_storage_sizes) {
            if let Some(data_range) =
                layer.validate_for_load(storage_size, self.host_page_size, scratch_base)?
            {
                data_end_gpa = data_end_gpa.max(data_range.end);
                data_ranges.push(data_range);
            }
            retained_bytes = retained_bytes
                .checked_add(storage_size)
                .ok_or_else(|| crate::new_error!("snapshot retained byte count overflows"))?;
            extent_count = extent_count
                .checked_add(layer.live_data.len())
                .ok_or_else(|| crate::new_error!("snapshot live-extent count overflows"))?;
            for range in &layer.live_data {
                mapped_bytes = mapped_bytes
                    .checked_add(range.end - range.start)
                    .ok_or_else(|| crate::new_error!("snapshot mapped byte count overflows"))?;
            }
        }
        validate_snapshot_totals(extent_count, mapped_bytes, retained_bytes)?;
        data_ranges.sort_unstable_by_key(|range| range.start);
        validate_sorted_snapshot_gpa_ranges(data_ranges)?;
        active_layer
            .page_tables
            .as_ref()
            .ok_or_else(|| crate::new_error!("active snapshot layer has no page tables"))?;
        let address_span = usize::try_from(
            data_end_gpa
                .checked_sub(base_gpa)
                .ok_or_else(|| crate::new_error!("snapshot address span starts below base"))?,
        )?;
        validate_snapshot_shape(
            self.stack_top_gva,
            self.entrypoint_addr,
            self.original_entrypoint_addr,
            &self.layout,
            address_span,
        )
    }
}

/// Fixed guest memory layout fields needed to rebuild a `SandboxMemoryLayout`.
/// Snapshot memory metadata is stored in `OciSnapshotConfig::layers`.
#[derive(Copy, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct MemoryLayout {
    pub(super) input_data_size: usize,
    pub(super) output_data_size: usize,
    pub(super) heap_size: usize,
    pub(super) code_size: usize,
    pub(super) init_data_size: usize,
    /// Memory region flag bits. `None` means default permissions.
    pub(super) init_data_permissions: Option<u32>,
    pub(super) scratch_size: usize,
}

/// Name and signature of one host function registered when the
/// snapshot was taken. The loader validates these against the
/// registry of the sandbox it is restoring into.
#[derive(Clone, Serialize, Deserialize)]
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

fn validate_platform(
    hyperlight_version: &str,
    arch: Arch,
    abi_version: u32,
    hypervisor: Hypervisor,
    cpu_vendor: &CpuVendor,
) -> crate::Result<()> {
    if arch != Arch::current() {
        return Err(crate::new_error!(
            "snapshot architecture mismatch: file is {:?}, current host is {:?} \
             (snapshot produced by hyperlight {})",
            arch,
            Arch::current(),
            hyperlight_version
        ));
    }
    if abi_version != SNAPSHOT_ABI_VERSION {
        return Err(crate::new_error!(
            "snapshot ABI version mismatch: file has version {}, this build expects {}. \
             The snapshot must be regenerated from the guest binary \
             (snapshot produced by hyperlight {}).",
            abi_version,
            SNAPSHOT_ABI_VERSION,
            hyperlight_version
        ));
    }
    let current_hv = Hypervisor::current()
        .ok_or_else(|| crate::new_error!("no hypervisor available to load snapshot"))?;
    if hypervisor != current_hv {
        return Err(crate::new_error!(
            "snapshot hypervisor mismatch: file was created on {} but the current hypervisor is {} \
             (snapshot produced by hyperlight {})",
            hypervisor.name(),
            current_hv.name(),
            hyperlight_version
        ));
    }
    let current_vendor = CpuVendor::current();
    if cpu_vendor != &current_vendor {
        return Err(crate::new_error!(
            "snapshot CPU vendor mismatch: file was created on {} but the current CPU is {} \
             (snapshot produced by hyperlight {})",
            cpu_vendor.as_str(),
            current_vendor.as_str(),
            hyperlight_version
        ));
    }
    Ok(())
}

fn validate_memory_layout(layout: &MemoryLayout) -> crate::Result<()> {
    // Bound untrusted sizes before rebuilding `SandboxMemoryLayout`.
    let max_region = SandboxMemoryLayout::MAX_MEMORY_SIZE;
    for (name, value) in [
        ("input_data_size", layout.input_data_size),
        ("output_data_size", layout.output_data_size),
        ("heap_size", layout.heap_size),
        ("code_size", layout.code_size),
        ("init_data_size", layout.init_data_size),
        ("scratch_size", layout.scratch_size),
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
    Ok(())
}

fn validate_snapshot_shape(
    stack_top_gva: u64,
    entrypoint_addr: u64,
    original_entrypoint_addr: u64,
    layout: &MemoryLayout,
    gpa_span_len: usize,
) -> crate::Result<()> {
    if gpa_span_len == 0 {
        return Err(crate::new_error!("snapshot GPA span must be nonzero"));
    }
    if !gpa_span_len.is_multiple_of(PAGE_SIZE) {
        return Err(crate::new_error!(
            "snapshot GPA span ({}) is not a multiple of PAGE_SIZE",
            gpa_span_len
        ));
    }

    // The dispatch entrypoint must remain in the executable code prefix.
    let code_lo = SandboxMemoryLayout::BASE_ADDRESS as u64;
    let code_hi = code_lo
        .checked_add(layout.code_size.next_multiple_of(PAGE_SIZE) as u64)
        .ok_or_else(|| {
            crate::new_error!(
                "snapshot layout overflow: BASE_ADDRESS + code_size ({}) does not fit in u64",
                layout.code_size
            )
        })?;
    if entrypoint_addr < code_lo || entrypoint_addr >= code_hi {
        return Err(crate::new_error!(
            "snapshot entrypoint addr {:#x} is outside the code region [{:#x}, {:#x})",
            entrypoint_addr,
            code_lo,
            code_hi
        ));
    }
    #[cfg(target_arch = "aarch64")]
    if !entrypoint_addr.is_multiple_of(4) {
        return Err(crate::new_error!(
            "snapshot entrypoint addr {:#x} is not 4-byte aligned",
            entrypoint_addr
        ));
    }

    // `AT_ENTRY` must remain within the captured GPA span.
    let snapshot_hi = code_lo.checked_add(gpa_span_len as u64).ok_or_else(|| {
        crate::new_error!(
            "snapshot layout overflow: BASE_ADDRESS + GPA span ({}) does not fit in u64",
            gpa_span_len
        )
    })?;
    let scratch_lo = hyperlight_common::layout::scratch_base_gpa(layout.scratch_size);
    if snapshot_hi > scratch_lo {
        return Err(crate::new_error!(
            "snapshot address span ends at {:#x}, above scratch base {:#x}",
            snapshot_hi,
            scratch_lo
        ));
    }
    if original_entrypoint_addr < code_lo || original_entrypoint_addr >= snapshot_hi {
        return Err(crate::new_error!(
            "snapshot original entrypoint addr {:#x} is outside the snapshot region [{:#x}, {:#x})",
            original_entrypoint_addr,
            code_lo,
            snapshot_hi
        ));
    }

    // The saved stack pointer must be aligned and inside guest memory.
    let max_gva = hyperlight_common::layout::SCRATCH_TOP_GVA as u64;
    if stack_top_gva == 0 || stack_top_gva > max_gva {
        return Err(crate::new_error!(
            "snapshot stack_top_gva {:#x} is outside the valid range (0, {:#x}]",
            stack_top_gva,
            max_gva
        ));
    }
    if !stack_top_gva.is_multiple_of(16) {
        return Err(crate::new_error!(
            "snapshot stack_top_gva {:#x} is not 16-byte aligned",
            stack_top_gva
        ));
    }
    Ok(())
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
            host_page_size: page_size::get(),
            stack_top_gva: 0x2000,
            entrypoint_addr: SandboxMemoryLayout::BASE_ADDRESS as u64,
            original_entrypoint_addr: SandboxMemoryLayout::BASE_ADDRESS as u64,
            sregs: distinct_sregs(),
            #[cfg(target_arch = "x86_64")]
            msrs: Vec::new(),
            layout: MemoryLayout {
                input_data_size: 0,
                output_data_size: 0,
                heap_size: 0,
                code_size: 0,
                init_data_size: 0,
                init_data_permissions: None,
                scratch_size: 0,
            },
            layers: Vec::new(),
            active_page_table_layer: 0,
            host_functions: Vec::new(),
            snapshot_generation: 0,
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
        let err = cfg.validate_for_load(&[]).unwrap_err().to_string();
        assert!(err.contains("architecture mismatch"), "got: {err}");
    }

    /// A snapshot stamped with a different ABI version is rejected.
    #[test]
    fn validate_for_load_rejects_abi_version_mismatch() {
        let mut cfg = gating_config();
        cfg.abi_version = SNAPSHOT_ABI_VERSION.wrapping_add(1);
        let err = cfg.validate_for_load(&[]).unwrap_err().to_string();
        assert!(err.contains("ABI version mismatch"), "got: {err}");
    }

    /// A snapshot captured under a different hypervisor backend is
    /// rejected. Without a live backend the load is rejected outright,
    /// which exercises the same gate from the other side.
    #[test]
    fn validate_for_load_rejects_hypervisor_mismatch() {
        let Some(current) = Hypervisor::current() else {
            let cfg = gating_config();
            let err = cfg.validate_for_load(&[]).unwrap_err().to_string();
            assert!(err.contains("no hypervisor available"), "got: {err}");
            return;
        };
        let other = [Hypervisor::Kvm, Hypervisor::Mshv, Hypervisor::Whp]
            .into_iter()
            .find(|h| *h != current)
            .expect("three backends, at least one differs from current");
        let mut cfg = gating_config();
        cfg.hypervisor = other;
        let err = cfg.validate_for_load(&[]).unwrap_err().to_string();
        assert!(err.contains("hypervisor mismatch"), "got: {err}");
    }
}

#[cfg(test)]
mod schema_pin {
    use super::*;

    #[cfg(target_arch = "x86_64")]
    const PINNED_CALL: &str = r#"{
  "hyperlight_version": "x.y.z",
  "arch": "x86_64",
    "abi_version": 3,
  "hypervisor": "mshv",
  "cpu_vendor": "intel",
    "host_page_size": 4096,
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
    "input_data_size": 1,
    "output_data_size": 2,
    "heap_size": 3,
    "code_size": 4,
    "init_data_size": 5,
        "init_data_permissions": null,
        "scratch_size": 8
  },
    "layers": [
        {
            "data": {
                "gpa_start": 16384,
                "len": 8192
            },
            "live_data": [
                {
                    "start": 0,
                    "end": 8192
                }
            ],
            "page_tables": {
                "start": 8192,
                "end": 12288
            }
        }
    ],
    "active_page_table_layer": 0,
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
    "abi_version": 3,
  "hypervisor": "mshv",
  "cpu_vendor": "intel",
    "host_page_size": 4096,
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
    "input_data_size": 1,
    "output_data_size": 2,
    "heap_size": 3,
    "code_size": 4,
    "init_data_size": 5,
        "init_data_permissions": null,
        "scratch_size": 8
  },
    "layers": [
        {
            "data": {
                "gpa_start": 16384,
                "len": 8192
            },
            "live_data": [
                {
                    "start": 0,
                    "end": 8192
                }
            ],
            "page_tables": {
                "start": 8192,
                "end": 12288
            }
        }
    ],
    "active_page_table_layer": 0,
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
