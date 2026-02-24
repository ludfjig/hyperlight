# Error Handling & Module Restructure — PR Plan

## Target state

### Public module structure (user-facing)

```
hyperlight_host::sandbox::UninitializedSandbox
hyperlight_host::sandbox::MultiUseSandbox
hyperlight_host::sandbox::GuestBinary
hyperlight_host::sandbox::SandboxConfiguration
hyperlight_host::sandbox::Snapshot
hyperlight_host::sandbox::InterruptHandle          (trait)
hyperlight_host::sandbox::Callable                 (trait)
hyperlight_host::sandbox::HostFunction
hyperlight_host::sandbox::Registerable             (trait)
hyperlight_host::sandbox::ParameterTuple           (trait, re-exported from hyperlight_common)
hyperlight_host::sandbox::SupportedReturnType      (trait, re-exported from hyperlight_common)
hyperlight_host::sandbox::ParameterValue           (re-exported from hyperlight_common)
hyperlight_host::sandbox::ReturnValue              (re-exported from hyperlight_common)
hyperlight_host::sandbox::ReturnType               (re-exported from hyperlight_common)
hyperlight_host::sandbox::is_hypervisor_present    (function)

hyperlight_host::sandbox::error::InternalError
hyperlight_host::sandbox::error::InvalidConfiguration
hyperlight_host::sandbox::error::CreateSandboxError
hyperlight_host::sandbox::error::EvolveError
hyperlight_host::sandbox::error::RegisterHostFunctionError
hyperlight_host::sandbox::error::CallError
hyperlight_host::sandbox::error::SnapshotError
hyperlight_host::sandbox::error::RestoreError
```

### File layout (crate internals)

```
src/hyperlight_host/src/
├── lib.rs
├── sandbox/
│   ├── mod.rs                  re-exports, module declarations
│   ├── builder.rs              UninitializedSandbox, GuestBinary, GuestEnvironment
│   ├── multi_use.rs            MultiUseSandbox
│   ├── callable.rs             Callable trait
│   ├── config.rs               SandboxConfiguration
│   ├── snapshot.rs             Snapshot
│   ├── error.rs                all public error types + InternalError
│   ├── host_func.rs            HostFunction, Registerable, TypeErasedHostFunction
│   ├── interrupt.rs            InterruptHandle trait
│   └── evolve.rs               evolve_impl_multi_use (internal wiring)
├── vm/                         pub(crate), replaces current hypervisor/
│   ├── mod.rs                  HyperlightVm struct, InterruptHandleImpl trait
│   ├── hyperlight_vm.rs        (or split into run.rs, io.rs, memory.rs later)
│   ├── error.rs                internal typed errors (DispatchGuestCallError, RunVmError, etc.)
│   ├── backend/                replaces current virtual_machine/
│   │   ├── mod.rs              VirtualMachine trait, HypervisorType, is_hypervisor_present()
│   │   ├── kvm.rs
│   │   ├── mshv.rs
│   │   └── whp.rs
│   ├── regs/
│   ├── gdb/
│   ├── crashdump.rs
│   ├── surrogate_process.rs
│   └── surrogate_process_manager.rs
├── mem/                        pub(crate)
├── metrics/                    pub(crate)
├── signal_handlers/            pub(crate)
└── testing/                    pub(crate), cfg(test)
```

---

## PR 1: Builder errors (UninitializedSandbox, config, host functions)

### What this PR does

Creates `sandbox/error.rs` with the error types needed by the sandbox builder path,
defines the `From` impls, and migrates all builder-side public methods to return
the new error types. `HyperlightError` stays public temporarily — the runtime
methods in `MultiUseSandbox` still use it. It will be made `pub(crate)` in PR 2
and deleted entirely in PR 4.

### Files created

- `src/hyperlight_host/src/sandbox/error.rs`

### Types defined in `sandbox/error.rs`

#### `InternalError`

Opaque wrapper for internal errors. Users can display/debug it but cannot match on inner cause.

```rust
#[derive(Debug)]
pub struct InternalError {
    source: Box<dyn std::error::Error + Send + Sync>,
    poisoning: bool,
}
```

Implements `Display` (delegates to `source`), `Error` (delegates `source()` to inner).
The `poisoning` flag is `pub(crate)` and tracks whether the original error was a
poisoning error. This is set during `From` conversions and later read by
`CallError::is_poison_error()` (added in PR 2).

Constructor: `InternalError::new(source, poisoning)` (pub(crate)).
Convenience: `From<HyperlightError> for InternalError` — sets `poisoning` from
`HyperlightError::is_poison_error()`.

#### `CreateSandboxError`

Returned by `UninitializedSandbox::new()`.

```rust
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum CreateSandboxError {
    #[error("No hypervisor was found")]
    NoHypervisorFound,

    #[error("Guest binary not found: '{path}'")]
    GuestBinaryNotFound {
        path: String,
        #[source]
        source: std::io::Error,
    },

    #[error(transparent)]
    Internal(InternalError),
}
```

`GuestBinaryNotFound` is promoted from the `GuestBinary::canonicalize()` path inside `new()`.
This is the most common user mistake (wrong path) and should be directly matchable.

#### `EvolveError`

Returned by `UninitializedSandbox::evolve()`.

```rust
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum EvolveError {
    #[error(transparent)]
    Internal(InternalError),
}
```

`NoHypervisorFound` is not included — `evolve()` is called after `new()` which
already checks for a hypervisor. All `evolve()` failures are internal infrastructure
errors (VM partition setup, memory manager, initialization).

#### `RegisterHostFunctionError`

Returned by `UninitializedSandbox::register()`, `register_print()`,
`Registerable::register_host_function()`.

```rust
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum RegisterHostFunctionError {
    #[error(transparent)]
    Internal(InternalError),
}
```

### `From` impls

- `From<HyperlightError> for CreateSandboxError` — promotes `NoHypervisorFound`; wraps rest as `Internal`.
  `GuestBinaryNotFound` is constructed explicitly in the `canonicalize()` call inside `new()`,
  not via a `From` impl (since it requires extracting the path and `io::Error` fields).
- `From<HyperlightError> for EvolveError` — wraps all as `Internal`
- `From<HyperlightError> for RegisterHostFunctionError` — wraps all as `Internal`
- `From<HyperlightError> for InternalError` — wraps, sets `poisoning` from `is_poison_error()`

### Signature changes

| Method | Before | After |
|--------|--------|-------|
| `UninitializedSandbox::new()` | `Result<Self>` | `Result<Self, CreateSandboxError>` |
| `UninitializedSandbox::evolve()` | `Result<MultiUseSandbox>` | `Result<MultiUseSandbox, EvolveError>` |
| `UninitializedSandbox::register()` | `Result<()>` | `Result<(), RegisterHostFunctionError>` |
| `UninitializedSandbox::register_print()` | `Result<()>` | `Result<(), RegisterHostFunctionError>` |
| `Registerable::register_host_function()` | `Result<()>` | `Result<(), RegisterHostFunctionError>` |
| `SandboxConfiguration::set_interrupt_vcpu_sigrtmin_offset()` | `crate::Result<()>` | `Result<(), InvalidConfiguration>` |
| `HostFunction::call()` | `Result<Output>` | `Result<Output, InternalError>` |

`GuestBinary::canonicalize()` is no longer a public method — fold its logic into
`UninitializedSandbox::new()` (the code already has a TODO for this). Make it `pub(crate)`
or inline it. Its errors surface through `CreateSandboxError::GuestBinaryNotFound`.

`InvalidConfiguration` is a simple struct error for input validation:

```rust
#[derive(Debug, Error)]
#[error("{message}")]
pub struct InvalidConfiguration {
    pub message: String,
}
```

### Implementation pattern

Internal code keeps using `HyperlightError` for now (removed in PR 4). At the public
boundary, `?` auto-converts via the `From` impls.

### Files modified

- `src/hyperlight_host/src/sandbox/mod.rs` — add `pub mod error;`

- `src/hyperlight_host/src/sandbox/uninitialized.rs`:
  - `UninitializedSandbox::new()` return type + body; inline `canonicalize()` logic,
    map file-not-found to `CreateSandboxError::GuestBinaryNotFound`
  - `UninitializedSandbox::evolve()` return type + body
  - `UninitializedSandbox::register()` return type + body
  - `UninitializedSandbox::register_print()` return type + body
  - `GuestBinary::canonicalize()` — make `pub(crate)` (was `pub`)

- `src/hyperlight_host/src/sandbox/config.rs`:
  - `SandboxConfiguration::set_interrupt_vcpu_sigrtmin_offset()` return type → `Result<(), InvalidConfiguration>`

- `src/hyperlight_host/src/func/host_functions.rs`:
  - `Registerable::register_host_function()` return type
  - `Registerable for UninitializedSandbox` impl return type + body
  - `HostFunction::call()` return type + body

- `src/hyperlight_host/src/sandbox/mod.rs`:
  - Update re-exports to include error types

### Tests

- Unit tests in `sandbox/error.rs` for each `From` impl (promotion + fallback-to-Internal)
- Update any tests in `uninitialized.rs` that match on `HyperlightError` for builder methods
- Doc examples on changed methods
- `just clippy debug && just clippy release`

### Notes

- `HyperlightError` stays `pub` in this PR — `MultiUseSandbox` methods still return it
- `pub type Result<T>` stays in `lib.rs` for the same reason
- `log_then_return!` and `new_error!` macros are untouched in this PR (both removed in PR 2)
- `HyperlightError` itself is removed entirely in PR 4

### Validation

```bash
just fmt-apply
just clippy debug
just clippy release
just guests
just test-like-ci
just test-like-ci release
```

---

## PR 2: Runtime errors (MultiUseSandbox, Callable)

### What this PR does

Adds `CallError`, `SnapshotError`, `RestoreError` to `sandbox/error.rs`. Migrates all
`MultiUseSandbox` and `Callable` methods to the new error types. Removes `HyperlightError`
from the public API (becomes `pub(crate)` temporarily — deleted entirely in PR 4).

### Types added to `sandbox/error.rs`

#### `CallError`

Returned by `MultiUseSandbox::call()`, `call_guest_function_by_name()`,
`call_type_erased_guest_function_by_name()`, and `Callable::call()`.

```rust
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum CallError {
    #[error("The sandbox is poisoned")]
    PoisonedSandbox,

    #[error("Guest aborted ({code}): {message}")]
    GuestAborted { code: u8, message: String },

    #[error("Execution was cancelled by the host")]
    ExecutionCanceled,

    #[error("Guest error ({code:?}): {message}")]
    GuestError { code: ErrorCode, message: String },

    #[error("Memory access violation at {addr:#x}")]
    MemoryAccessViolation { addr: u64 },

    #[error("Non-executable address {addr:#x} tried to be executed")]
    ExecutionAccessViolation { addr: u64 },

    #[error(transparent)]
    Internal(InternalError),
}
```

#### `SnapshotError`

```rust
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum SnapshotError {
    #[error("The sandbox is poisoned")]
    PoisonedSandbox,

    #[error(transparent)]
    Internal(InternalError),
}
```

#### `RestoreError`

```rust
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum RestoreError {
    #[error("Snapshot was taken from a different sandbox")]
    SnapshotMismatch,

    #[error(transparent)]
    Internal(InternalError),
}
```

### `From` impls added

- `From<HyperlightError> for CallError` — promotes `GuestAborted`, `ExecutionCanceledByHost`,
  `GuestError`, `MemoryAccessViolation`, `ExecutionAccessViolation`, `PoisonedSandbox`; wraps rest as `Internal`.
  **Note:** `HyperlightError::MemoryAccessViolation(u64, MemoryRegionFlags, MemoryRegionFlags)` has three
  fields but `CallError::MemoryAccessViolation { addr }` keeps only the address. The two `MemoryRegionFlags`
  values are intentionally dropped to avoid leaking internal types into the public API.
- `From<DispatchGuestCallError> for CallError` — same promotion logic as current `promote()` method
- `From<HyperlightError> for SnapshotError` — promotes `PoisonedSandbox`; wraps rest
- `From<HyperlightError> for RestoreError` — promotes `SnapshotSandboxMismatch`; wraps rest

### `CallError` poisoning

Add `CallError::is_poison_error()` method (pub(crate)) that returns true for:
- `GuestAborted`
- `ExecutionCanceled`
- `MemoryAccessViolation`
- `ExecutionAccessViolation`
- `Internal(e)` where `e.poisoning` is true

### Signature changes

| Method | Before | After |
|--------|--------|-------|
| `Callable::call()` | `Result<Output>` | `Result<Output, CallError>` |
| `MultiUseSandbox::call()` | `Result<Output>` | `Result<Output, CallError>` |
| `MultiUseSandbox::call_guest_function_by_name()` | `Result<Output>` | `Result<Output, CallError>` |
| `MultiUseSandbox::call_type_erased_guest_function_by_name()` | `Result<ReturnValue>` | `Result<ReturnValue, CallError>` |
| `MultiUseSandbox::snapshot()` | `Result<Arc<Snapshot>>` | `Result<Arc<Snapshot>, SnapshotError>` |
| `MultiUseSandbox::restore()` | `Result<()>` | `Result<(), RestoreError>` |
| `MultiUseSandbox::map_file_cow()` | `Result<u64>` | `Result<u64, CallError>` |
| `MultiUseSandbox::generate_crashdump()` | `Result<()>` | `Result<(), InternalError>` |

### Implementation pattern

Same as PR 1. Internal code keeps using `HyperlightError`/`DispatchGuestCallError`
for now (these are removed in PR 4 when per-module errors replace them).
At the public boundary, `?` auto-converts via the `From` impls.

For `call_guest_function_by_name_no_reset` (the internal workhorse), it can either:
- Stay returning `Result<ReturnValue, HyperlightError>` and convert at the public boundary
- Or switch to `Result<ReturnValue, CallError>` internally — either is fine

The poisoning logic in `call_guest_function_by_name_no_reset` currently uses
`HyperlightError::is_poison_error()`. This changes to `CallError::is_poison_error()` since
the method now returns `CallError`.

**Important:** The `self.poisoned |= should_poison` / `self.poisoned |= e.is_poison_error()`
assignment inside `call_guest_function_by_name_no_reset` stays as manual code. The `From` impl
handles variant promotion but **cannot mutate sandbox state** — poisoning the sandbox is the
caller's responsibility and must remain an explicit assignment in the method body.

### Files modified

- `src/hyperlight_host/src/sandbox/error.rs`:
  - Add `CallError`, `SnapshotError`, `RestoreError`
  - Add `From` impls
  - Add `CallError::is_poison_error()`

- `src/hyperlight_host/src/sandbox/initialized_multi_use.rs`:
  - `snapshot()` return type + body
  - `restore()` return type + body
  - `call()` return type + body
  - `call_guest_function_by_name()` return type + body
  - `call_type_erased_guest_function_by_name()` return type + body
  - `call_guest_function_by_name_no_reset()` return type or boundary conversion
  - `map_file_cow()` return type + body
  - `generate_crashdump()` return type + body
  - `Callable for MultiUseSandbox` impl
  - All tests updated to match on new error types

- `src/hyperlight_host/src/sandbox/callable.rs`:
  - `Callable::call()` return type

- `src/hyperlight_host/src/lib.rs`:
  - Remove `pub use error::HyperlightError;`
  - Remove `pub type Result<T> = core::result::Result<T, error::HyperlightError>;`
  - Remove the `log_then_return!` macro definition
  - Keep `pub(crate) type Result<T>` for internal use, or remove and update all internal callers

- `src/hyperlight_host/src/error.rs`:
  - Change `pub enum HyperlightError` to `pub(crate) enum HyperlightError`
    (temporary — deleted entirely in PR 4)
  - Remove the `new_error!` macro definition

- Fuzz targets (update imports and error matching):
  - `fuzz/fuzz_targets/host_call.rs` — uses `HyperlightError` directly; switch to new error types
  - `fuzz/fuzz_targets/guest_call.rs` — uses `func::ParameterValue`, `func::ReturnType`
  - `fuzz/fuzz_targets/guest_trace.rs` — uses `func::ParameterValue`, `func::ReturnType`, `func::ReturnValue`
  - `fuzz/fuzz_targets/host_print.rs` — uses `sandbox::uninitialized::GuestBinary`
  (The `func::` and `sandbox::uninitialized::` paths will still work via PR 3 deprecated shims,
   but update them here to use canonical paths if convenient.)

- Examples (update imports and error types):
  - `src/hyperlight_host/examples/guest-debugging/main.rs` — uses `hyperlight_host::{Result, new_error}`
    (11 call sites of `new_error!`). Since `HyperlightError` is now `pub(crate)`, `new_error!` can no
    longer be used in examples. Replace with `Box<dyn Error>` or a local error type.
  - `src/hyperlight_host/examples/tracing-otlp/main.rs` — uses `HyperlightError`, `Result`
  - `src/hyperlight_host/examples/tracing-chrome.rs` — uses `Result`
  - `src/hyperlight_host/examples/tracing.rs` — uses `Result`
  - `src/hyperlight_host/examples/logging.rs` — uses `Result`
  - `src/hyperlight_host/examples/metrics.rs` — uses `Result`
  - `src/hyperlight_host/examples/func_ctx.rs` — uses `GuestBinary`, `sandbox::UninitializedSandbox`

### Macro removal: `log_then_return!` and `new_error!`

Both macros are removed entirely in this PR. Neither is type-safe — they both
produce `HyperlightError::Error(String)`, a catch-all variant with no structured information.

#### `log_then_return!`

Currently expands to:
```rust
tracing::error!($($arg)*);
return Err($crate::new_error!($($arg)*));
```

20 call sites — each replaced with explicit `tracing::error!` +
`return Err(HyperlightError::Error(format!(...)))` (or a more specific error variant
where one exists):

| File | Call sites |
|------|------------|
| `vm/gdb/mod.rs` | 1 |
| `vm/surrogate_process_manager.rs` | 3 |
| `vm/surrogate_process.rs` | 2 |
| `sandbox/multi_use.rs` (was `initialized_multi_use.rs`) | 4 (in `map_file_cow`) |
| `mem/shared_mem.rs` | 5 |
| `mem/shared_mem_tests.rs` | 2 |
| `mem/elf.rs` | 3 |

Macro definition in `lib.rs` is deleted.

#### `new_error!`

Currently expands to `HyperlightError::Error(format!(...))`. ~80 call sites across
19 files. Each is replaced with `HyperlightError::Error(format!(...))` inline.

| File | Call sites |
|------|------------|
| `mem/shared_mem.rs` | 16 |
| `sandbox/uninitialized.rs` (→ `builder.rs`) | 13 |
| `sandbox/trace/context.rs` | 8 |
| `mem/layout.rs` | 7 |
| `hypervisor/surrogate_process_manager.rs` (→ `vm/`) | 4 |
| `func/host_functions.rs` (→ `sandbox/host_func.rs`) | 3 |
| `testing/log_values.rs` | 3 |
| `hypervisor/crashdump.rs` (→ `vm/`) | 3 |
| `sandbox/mod.rs` | 2 |
| `sandbox/trace/mem_profile.rs` | 2 |
| `sandbox/uninitialized_evolve.rs` (→ `evolve.rs`) | 1 |
| `sandbox/config.rs` | 1 |
| `mem/shared_mem_tests.rs` | 1 |
| `mem/mgr.rs` | 1 |
| `mem/elf.rs` | 1 |
| `hypervisor/mod.rs` (→ `vm/`) | 1 |
| `hypervisor/gdb/mod.rs` (→ `vm/`) | 1 |
| `tests/sandbox_host_tests.rs` | 2 |
| `examples/guest-debugging/main.rs` | 11 |

The `examples/guest-debugging/main.rs` case is special: since `HyperlightError` becomes
`pub(crate)`, the example can no longer construct it. Replace with `Box<dyn Error>` as the
error type and use `format!("...").into()` or `anyhow::anyhow!("...")` instead.

Macro definition in `error.rs` is deleted.

**Note:** The `HyperlightError::Error(String)` variant itself stays temporarily — it is
the inline replacement target for `new_error!` sites. The entire `HyperlightError` enum
(including `Error(String)`) is deleted in PR 4 when per-module internal errors replace it.

### Tests to update

- All tests in `initialized_multi_use.rs` that match on `HyperlightError::GuestAborted`,
  `HyperlightError::PoisonedSandbox`, `HyperlightError::GuestError`,
  `HyperlightError::MemoryAccessViolation`, `HyperlightError::SnapshotSandboxMismatch`
  → match on `CallError`, `SnapshotError`, `RestoreError` variants instead

- Tests in `error.rs` — these test internal machinery (`promote()`, `is_poison_error()`),
  they stay as-is since they test `pub(crate)` code

- Unit tests in `sandbox/error.rs` for each new `From` impl + `CallError::is_poison_error()`

- Doc examples on all changed methods

### Validation

```bash
just fmt-apply
just clippy debug
just clippy release
just guests
just test-like-ci
just test-like-ci release
```

---

## PR 3: Module restructure (non-breaking, with deprecated shims)

### What this PR does

Renames and moves files to match the target folder structure. Adds deprecated
re-export modules so that all old public paths still compile (with warnings).
No logic changes — purely mechanical path updates + compatibility shims.

### File renames/moves

| From | To |
|------|----|
| `src/hyperlight_host/src/hypervisor/` | `src/hyperlight_host/src/vm/` |
| `src/hyperlight_host/src/hypervisor/virtual_machine/` | `src/hyperlight_host/src/vm/backend/` |
| `src/hyperlight_host/src/hypervisor/virtual_machine/mod.rs` | `src/hyperlight_host/src/vm/backend/mod.rs` |
| `src/hyperlight_host/src/hypervisor/virtual_machine/kvm.rs` | `src/hyperlight_host/src/vm/backend/kvm.rs` |
| `src/hyperlight_host/src/hypervisor/virtual_machine/mshv.rs` | `src/hyperlight_host/src/vm/backend/mshv.rs` |
| `src/hyperlight_host/src/hypervisor/virtual_machine/whp.rs` | `src/hyperlight_host/src/vm/backend/whp.rs` |
| `src/hyperlight_host/src/hypervisor/hyperlight_vm.rs` | `src/hyperlight_host/src/vm/hyperlight_vm.rs` |
| `src/hyperlight_host/src/hypervisor/mod.rs` | `src/hyperlight_host/src/vm/mod.rs` |
| `src/hyperlight_host/src/hypervisor/regs.rs` | `src/hyperlight_host/src/vm/regs.rs` |
| `src/hyperlight_host/src/hypervisor/regs/` | `src/hyperlight_host/src/vm/regs/` |
| `src/hyperlight_host/src/hypervisor/gdb/` | `src/hyperlight_host/src/vm/gdb/` |
| `src/hyperlight_host/src/hypervisor/crashdump.rs` | `src/hyperlight_host/src/vm/crashdump.rs` |
| `src/hyperlight_host/src/hypervisor/wrappers.rs` | `src/hyperlight_host/src/vm/wrappers.rs` |
| `src/hyperlight_host/src/hypervisor/surrogate_process.rs` | `src/hyperlight_host/src/vm/surrogate_process.rs` |
| `src/hyperlight_host/src/hypervisor/surrogate_process_manager.rs` | `src/hyperlight_host/src/vm/surrogate_process_manager.rs` |
| `src/hyperlight_host/src/sandbox/initialized_multi_use.rs` | `src/hyperlight_host/src/sandbox/multi_use.rs` |
| `src/hyperlight_host/src/sandbox/uninitialized.rs` | `src/hyperlight_host/src/sandbox/builder.rs` |
| `src/hyperlight_host/src/sandbox/uninitialized_evolve.rs` | `src/hyperlight_host/src/sandbox/evolve.rs` |
| `src/hyperlight_host/src/sandbox/outb.rs` | `src/hyperlight_host/src/vm/io.rs` |
| `src/hyperlight_host/src/func/host_functions.rs` | `src/hyperlight_host/src/sandbox/host_func.rs` |

### New files created

- `src/hyperlight_host/src/sandbox/interrupt.rs` — move the `InterruptHandle` trait definition
  from `hypervisor/mod.rs` (now `vm/mod.rs`) into this file. Import it back in `vm/mod.rs`
  with `use crate::sandbox::interrupt::InterruptHandle`.

### Deprecated compatibility shims in `lib.rs`

Instead of deleting old public modules, replace them with deprecated re-export shims.
This keeps old import paths working with deprecation warnings.

#### `hypervisor` shim

```rust
/// Deprecated: use `hyperlight_host::sandbox` instead.
#[deprecated(since = "X.Y.0", note = "use `hyperlight_host::sandbox::InterruptHandle` instead")]
pub mod hypervisor {
    /// Deprecated: use `hyperlight_host::sandbox` instead.
    #[deprecated(since = "X.Y.0", note = "use `hyperlight_host::sandbox::is_hypervisor_present` instead")]
    pub mod virtual_machine {
        pub use crate::sandbox::is_hypervisor_present;
    }
    pub use crate::sandbox::InterruptHandle;
}
```

#### `func` shim

```rust
/// Deprecated: use `hyperlight_host::sandbox` instead.
#[deprecated(since = "X.Y.0", note = "use types from `hyperlight_host::sandbox` instead")]
pub mod func {
    pub use crate::sandbox::{
        HostFunction, ParameterTuple, ParameterValue, Registerable,
        ReturnType, ReturnValue, SupportedReturnType,
    };
    // These are re-exported here for backward compat only — they are not part of the
    // target public API in `sandbox` and will be removed when this shim is deleted in PR 4.
    pub use hyperlight_common::func::{ResultType, SupportedParameterType};
}
```

#### `error` shim (after PR 2 made `HyperlightError` `pub(crate)`)

The `error` module currently has nothing useful to re-export publicly after PR 2.
It becomes `pub(crate) mod error` in this PR. If any downstream code imported from
`hyperlight_host::error::HyperlightError`, that path was already removed in PR 2
(the breaking change). No shim needed.

#### Old `lib.rs` re-exports

Keep the existing top-level re-exports as deprecated:

```rust
#[deprecated(since = "X.Y.0", note = "use `hyperlight_host::sandbox::is_hypervisor_present` instead")]
pub use sandbox::is_hypervisor_present;

// These re-exports stay without deprecation (they point to the new canonical locations):
pub use sandbox::MultiUseSandbox;
pub use sandbox::UninitializedSandbox;
pub use sandbox::GuestBinary;
```

### `sandbox/mod.rs` re-exports

Add the new canonical re-exports in `sandbox/mod.rs`:

```rust
pub use crate::vm::backend::is_hypervisor_present;
```

And add the submodule for `uninitialized` as a deprecated shim:

```rust
/// Deprecated: use `hyperlight_host::sandbox::GuestBinary` instead.
#[deprecated(since = "X.Y.0", note = "use `hyperlight_host::sandbox::GuestBinary` instead")]
pub mod uninitialized {
    pub use super::builder::GuestBinary;
    pub use super::builder::UninitializedSandbox;
}
```

Similarly for `initialized_multi_use`:

```rust
/// Deprecated: use `hyperlight_host::sandbox::MultiUseSandbox` instead.
#[deprecated(since = "X.Y.0", note = "use `hyperlight_host::sandbox::MultiUseSandbox` instead")]
pub mod initialized_multi_use {
    pub use super::multi_use::MultiUseSandbox;
}
```

### Modules deleted

- `src/hyperlight_host/src/func/` — real code absorbed into `sandbox/host_func.rs`;
  replaced by the deprecated shim module above

### Files modified (path updates)

Every file in the crate that has `use crate::hypervisor::` needs updating to `use crate::vm::`.
Every file that has `use crate::func::` needs updating to `use crate::sandbox::host_func::` or
the re-export path.

Key files with many path updates:
- `src/hyperlight_host/src/lib.rs` — `mod hypervisor` → `mod vm`, add deprecated shim modules
- `src/hyperlight_host/src/sandbox/mod.rs` — update re-exports, add `mod host_func`, `mod interrupt`,
  add deprecated submodule shims
- `src/hyperlight_host/src/vm/mod.rs` — all internal `use` paths
- `src/hyperlight_host/src/vm/hyperlight_vm.rs` — all internal `use` paths
- `src/hyperlight_host/src/sandbox/multi_use.rs` — all internal `use` paths
- `src/hyperlight_host/src/sandbox/builder.rs` — all internal `use` paths
- `src/hyperlight_host/src/sandbox/evolve.rs` — all internal `use` paths
- `src/hyperlight_host/src/mem/mgr.rs` — if it references hypervisor types
- `src/hyperlight_host/src/error.rs` — update `use crate::hypervisor::` paths
- `fuzz/fuzz_targets/*.rs` — if they reference crate paths directly

### Validation

```bash
just fmt-apply
just clippy debug
just clippy release
just guests
just test-like-ci
just test-like-ci release
```

---

## PR 4: Delete `HyperlightError`, tighten visibility

### What this PR does

Deletes the `HyperlightError` enum entirely, replacing it with per-module internal
error types (`MemError`, `VmError`, etc.). Tightens visibility on internal modules.

**Deprecated shims from PR 3 are kept.** They will be removed in a future release
(at least a couple of cargo releases after PR 3 ships).

### Visibility changes in `lib.rs`

- Change `pub mod mem` → `pub(crate) mod mem`
- Change `pub mod metrics` → `pub(crate) mod metrics`
- Deprecated shim modules (`pub mod hypervisor`, `pub mod func`) stay
- Deprecated `pub use sandbox::is_hypervisor_present` stays

```rust
pub mod error;

mod builder;
mod callable;
mod config;
mod evolve;
mod host_func;
mod interrupt;
mod multi_use;
mod snapshot;

pub use builder::{UninitializedSandbox, GuestBinary};
pub use callable::Callable;
pub use config::SandboxConfiguration;
pub use host_func::{HostFunction, Registerable};
pub use interrupt::InterruptHandle;
pub use multi_use::MultiUseSandbox;
pub use snapshot::Snapshot;

pub use crate::vm::backend::is_hypervisor_present;

pub use hyperlight_common::func::{ParameterTuple, SupportedReturnType};
pub use hyperlight_common::flatbuffer_wrappers::function_types::{
    ParameterValue, ReturnType, ReturnValue,
};
```

### Visibility changes in `mem/mod.rs`

All submodules become `pub(crate)`:

| Before | After |
|--------|-------|
| `pub mod layout` | `pub(crate) mod layout` |
| `pub mod memory_region` | `pub(crate) mod memory_region` |
| `pub mod mgr` | `pub(crate) mod mgr` |
| `pub mod ptr` | `pub(crate) mod ptr` |
| `pub mod ptr_offset` | `pub(crate) mod ptr_offset` |
| `pub mod shared_mem` | `pub(crate) mod shared_mem` |

### Items to remove

- `pub use error::HyperlightError` from `lib.rs` (already done in PR 2, verify)
- `pub type Result<T>` from `lib.rs` (already done in PR 2, verify)
- `pub(crate) mod error` from `lib.rs` — entire module deleted
- `src/hyperlight_host/src/error.rs` — file deleted
- Any remaining `pub` on types that are no longer reachable through public paths
  (find with `cargo doc --document-private-items` and audit)

### `HyperlightError` removal

The entire `HyperlightError` enum and `error.rs` file are deleted. Every internal
function that currently returns `Result<T, HyperlightError>` is migrated to return
its module's own error type instead.

#### Per-module internal error types

| Module | Error type | Key variants (moved from `HyperlightError`) |
|--------|-----------|---------------------------------------------|
| `mem/` | `MemError` | `BoundsCheckFailed`, `CheckedAddOverflow`, `MemoryAccessViolation`, `MemoryAllocationFailed`, `MemoryProtectionFailed`, `MemoryRegionSizeMismatch`, `MemoryRequestTooBig`, `MmapFailed`, `MprotectFailed`, `RawPointerLessThanBaseAddress`, `GuestOffsetIsInvalid`, `NoMemorySnapshot`, `SnapshotSizeMismatch`, `VectorCapacityIncorrect` |
| `vm/` | `VmError` (extend existing `HyperlightVmError`) | `ExecutionAccessViolation`, `ExecutionCanceledByHost`, `NoHypervisorFound`, `VmmSysError`, `WindowsAPIError`, plus existing sub-errors (`DispatchGuestCallError`, `RunVmError`, etc.) |
| `sandbox/` (internal) | Inline in each function or small per-file enums | `GuestAborted`, `GuestError`, `PoisonedSandbox`, `GuestExecutionHungOnHostFunctionCall`, `GuestFunctionCallAlreadyInProgress`, `HostFunctionNotFound`, `SnapshotSandboxMismatch` |
| `func/` → `sandbox/host_func.rs` | Absorbed into `sandbox/` errors or `HostFuncError` | `GuestInterfaceUnsupportedType`, `UnexpectedNoOfArguments`, `UnexpectedParameterValueType`, `UnexpectedReturnValueType`, `ParameterValueConversionFailure`, `ReturnValueConversionFailure`, `FailedToGetValueFromParameter` |

Variants that are thin wrappers around stdlib types (`IOError`, `IntConversionFailure`,
`UTF8StringConversionFailure`, `TryFromSliceError`, `JsonConversionFailure`,
`CStringConversionError`, `SystemTimeError`, `RefCellBorrowFailed`, `RefCellMutBorrowFailed`,
`PEFileProcessingFailure`, `CrossBeamReceiveError`, `CrossBeamSendError`, `LockAttemptFailed`,
`AnyhowError`) are **not** moved to separate enums — they become `#[from]` sources or
`.map_err(...)` conversions into whichever module error is used at the call site.

The catch-all `Error(String)` variant (which all `new_error!` sites were inlined to in PR 2)
is eliminated by replacing each occurrence with:
- A typed variant in the appropriate module error, or
- `anyhow::anyhow!("...")` wrapped into the module error's `Internal`/`Other` variant

#### Poisoning

The exhaustive `is_poison_error()` match is no longer needed on a central enum.
Each module-level error that can poison implements:

```rust
pub(crate) fn is_poison_error(&self) -> bool { ... }
```

This already exists on `DispatchGuestCallError`. `MemError` gets the same for
`MemoryAccessViolation`, `SnapshotSizeMismatch`, `MemoryRegionSizeMismatch`.
The public `CallError::is_poison_error()` delegates to the inner source when
it wraps an `InternalError`.

#### `From` impls at module boundaries

Internal code chains errors across modules with explicit `From` impls:

```
MemError ──► VmError    (via From, for vm code that calls mem functions)
VmError  ──► CallError  (via From, at the public boundary)
MemError ──► CallError  (via From, for sandbox code that calls mem directly)
```

#### Migration strategy

File by file:
1. Create `mem/error.rs` with `MemError`, add `From` impls from stdlib types
2. Migrate `mem/*.rs` functions: `Result<T, HyperlightError>` → `Result<T, MemError>`
3. Extend `vm/error.rs`: absorb VM-related variants into `VmError`
4. Migrate `vm/*.rs` functions: `Result<T, HyperlightError>` → `Result<T, VmError>`
5. Migrate `sandbox/*.rs` internal functions: use local error types or `VmError`/`MemError`
6. Delete `error.rs` and `pub(crate) mod error` from `lib.rs`
7. Update all `From<HyperlightError> for PublicError` impls to `From<ModuleError> for PublicError`

### Other cleanup

- Remove `DispatchGuestCallError::promote()` — its logic lives in `From<DispatchGuestCallError> for CallError`
- Audit and clean up doc comments referencing old paths (e.g., `crate::HyperlightError::PoisonedSandbox`)
- Update README and docs/ if they reference old import paths

### `lib.rs` final state

```rust
pub mod sandbox;

// Deprecated shims — kept for backward compat, removed in a future release
#[deprecated] pub mod hypervisor { /* ... */ }
#[deprecated] pub mod func { /* ... */ }
#[deprecated] pub use sandbox::is_hypervisor_present;
pub use sandbox::{MultiUseSandbox, UninitializedSandbox, GuestBinary};

pub(crate) mod vm;
pub(crate) mod mem;
pub(crate) mod metrics;
#[cfg(target_os = "linux")]
pub(crate) mod signal_handlers;
#[cfg(test)]
pub(crate) mod testing;
```

Note: `pub(crate) mod error` is gone — no central error module exists.
Deprecated shims and `sandbox/mod.rs` submodule shims remain until a future release.

### Validation

```bash
just fmt-apply
just clippy debug
just clippy release
just guests
just test-like-ci
just test-like-ci release
```

Verify that `cargo doc` generates clean docs with only `sandbox` module + deprecated shims visible.
Verify that downstream code (examples, fuzz targets) compiles with new import paths.

---

## PR 5: `hyperlight_common` cleanup — replace `anyhow`, restructure modules

### What this PR does

Removes the `anyhow` dependency from `hyperlight_common` (a `no_std` library crate where
`anyhow` is inappropriate) and replaces it with a typed `CommonError` enum. Restructures
modules so the public API describes *what* it contains rather than *how* it's serialized
(`flatbuffer_wrappers` → proper domain modules). Adds deprecated re-export shims for
backward compatibility.

### Replace `anyhow` with `CommonError`

The crate already depends on `thiserror` (which supports `no_std`). Define a crate-level
typed error in `src/hyperlight_common/src/error.rs`:

```rust
use alloc::string::String;

#[derive(Debug, thiserror::Error)]
pub enum CommonError {
    #[error("invalid flatbuffer: {0}")]
    InvalidFlatbuffer(&'static str),

    #[error("missing field: {0}")]
    MissingField(&'static str),

    #[error("unexpected enum variant: {0}")]
    UnexpectedVariant(&'static str),

    #[error("type conversion failed: expected {expected}, got {got}")]
    TypeConversion {
        expected: &'static str,
        got: String,
    },
}
```

This covers all ~65 `anyhow` sites which fall into exactly these 4 categories:
1. **InvalidFlatbuffer** — `anyhow!("Error while reading ...")` on flatbuffer parse failures
2. **MissingField** — `anyhow!("Missing field: ...")` in field extraction
3. **UnexpectedVariant** — `bail!("Unsupported ...")` / `anyhow!("Unknown ...")` for unknown enum discriminants
4. **TypeConversion** — `bail!("Unexpected ParameterValue ...")` for wrong value types

All `TryFrom` impls switch from `type Error = anyhow::Error` to `type Error = CommonError`.

### `anyhow` removal sites

| File | Sites | Pattern |
|------|-------|---------|
| `outb.rs` | 3 | `TryFrom<u8> for Exception`, `TryFrom<u16> for OutBAction` |
| `flatbuffer_wrappers/function_call.rs` | 5 | `TryFrom`, validation fns, `bail!` |
| `flatbuffer_wrappers/function_types.rs` | ~30 | Every `TryFrom` conversion (~20 impls) |
| `flatbuffer_wrappers/guest_log_data.rs` | 5 | `TryFrom` deserialization |
| `flatbuffer_wrappers/guest_log_level.rs` | 1 | `TryFrom<&FbLogLevel>` |
| `flatbuffer_wrappers/guest_trace_data.rs` | ~12 | `TryFrom`, batch decoding |
| `flatbuffer_wrappers/host_function_definition.rs` | 6 | `TryFrom`, verification |
| `flatbuffer_wrappers/host_function_details.rs` | 3 | `TryFrom` deserialization |

After migration, remove `anyhow` from `Cargo.toml`.

### Module restructure

| Current path | New path | What moves |
|-------------|----------|------------|
| `flatbuffer_wrappers/function_types.rs` | `func/types.rs` | `ParameterValue`, `ReturnValue`, `ReturnType`, `ParameterType`, `FunctionCallResult` |
| `flatbuffer_wrappers/function_call.rs` | `func/call.rs` | `FunctionCall`, `FunctionCallType`, validation fns |
| `flatbuffer_wrappers/host_function_definition.rs` | `func/definition.rs` | `HostFunctionDefinition` |
| `flatbuffer_wrappers/host_function_details.rs` | `func/details.rs` | `HostFunctionDetails` |
| `flatbuffer_wrappers/util.rs` | `func/serialize.rs` | `get_flatbuffer_result`, `FlatbufferSerializable`, `estimate_flatbuffer_capacity` (make `pub(crate)` where possible) |
| `flatbuffer_wrappers/guest_error.rs` | `guest/error.rs` | `ErrorCode`, `GuestError` |
| `flatbuffer_wrappers/guest_log_data.rs` + `guest_log_level.rs` | `guest/log.rs` | `GuestLogData`, `LogLevel` |
| `flatbuffer_wrappers/guest_trace_data.rs` | `guest/trace.rs` | `GuestEvent`, `EventsEncoder`, `EventsDecoder`, etc. |
| `flatbuffer_wrappers/mod.rs` | Deleted | Replaced by deprecated shim |

### Target file layout

```
src/hyperlight_common/src/
├── lib.rs
├── error.rs                CommonError
├── func/
│   ├── mod.rs              re-exports: ParameterValue, ReturnValue, ReturnType, etc.
│   ├── types.rs            ParameterValue, ReturnValue, ReturnType, ParameterType, FunctionCallResult
│   ├── call.rs             FunctionCall, FunctionCallType, validation
│   ├── definition.rs       HostFunctionDefinition
│   ├── details.rs          HostFunctionDetails
│   ├── serialize.rs        flatbuffer serialization helpers
│   ├── error.rs            func::Error (existing, thiserror — unchanged)
│   ├── functions.rs        Function trait (unchanged)
│   ├── param_type.rs       ParameterTuple, SupportedParameterType (unchanged)
│   ├── ret_type.rs         ResultType, SupportedReturnType (unchanged)
│   └── utils.rs            for_each_tuple! macro (unchanged)
├── guest/
│   ├── mod.rs              re-exports: ErrorCode, GuestError, GuestLogData, LogLevel
│   ├── error.rs            ErrorCode, GuestError
│   ├── log.rs              GuestLogData, LogLevel
│   └── trace.rs            GuestEvent, EventsEncoder, etc. (cfg trace_guest)
├── layout.rs               (unchanged)
├── mem.rs                  (unchanged)
├── outb.rs                 (TryFrom uses CommonError now)
├── resource.rs             (unchanged)
└── vmem.rs                 (unchanged)
```

### Import updates across all downstream crates

Since `hyperlight_common` is only consumed by in-workspace crates (not external users),
no deprecated re-export shim is needed. The `flatbuffer_wrappers` module is simply
deleted and all imports are updated directly in the same PR.

Key mapping (applies to all crates):

| Old import | New import |
|-----------|-----------|
| `flatbuffer_wrappers::function_types::{ParameterValue, ReturnValue, ...}` | `func::{ParameterValue, ReturnValue, ...}` |
| `flatbuffer_wrappers::function_call::{FunctionCall, ...}` | `func::{FunctionCall, ...}` |
| `flatbuffer_wrappers::guest_error::{ErrorCode, GuestError}` | `guest::{ErrorCode, GuestError}` |
| `flatbuffer_wrappers::guest_log_data::GuestLogData` | `guest::{GuestLogData}` |
| `flatbuffer_wrappers::guest_log_level::LogLevel` | `guest::{LogLevel}` |
| `flatbuffer_wrappers::guest_trace_data::*` | `guest::trace::*` |
| `flatbuffer_wrappers::host_function_definition::*` | `func::{HostFunctionDefinition}` |
| `flatbuffer_wrappers::host_function_details::*` | `func::{HostFunctionDetails}` |
| `flatbuffer_wrappers::util::*` | `func::serialize::*` |

#### `hyperlight_host` (~49 import sites)

Largest consumer — imports spread across `sandbox/`, `hypervisor/`, `func/`, `mem/`, etc.
All `flatbuffer_wrappers::*` paths updated to `func::` / `guest::` equivalents.

#### `hyperlight_guest` (10 `flatbuffer_wrappers` imports)

| File | Imports |
|------|---------|
| `error.rs` | `guest_error::ErrorCode` → `guest::ErrorCode` |
| `arch/amd64/prim_alloc.rs` | `guest_error::ErrorCode` → `guest::ErrorCode` |
| `guest_handle/host_comm.rs` | `function_call::{FunctionCall, FunctionCallType}` → `func::{FunctionCall, FunctionCallType}` |
| `guest_handle/host_comm.rs` | `function_types::{ParameterValue, ReturnType, ReturnValue}` → `func::{ParameterValue, ReturnType, ReturnValue}` |
| `guest_handle/host_comm.rs` | `guest_error::ErrorCode` → `guest::ErrorCode` |
| `guest_handle/host_comm.rs` | `guest_log_data::GuestLogData` → `guest::GuestLogData` |
| `guest_handle/host_comm.rs` | `guest_log_level::LogLevel` → `guest::LogLevel` |
| `guest_handle/host_comm.rs` | `util::estimate_flatbuffer_capacity` → `func::serialize::estimate_flatbuffer_capacity` |
| `guest_handle/io.rs` | `guest_error::ErrorCode` → `guest::ErrorCode` |

Note: `hyperlight_guest` is `no_std` — all new paths must remain `no_std` compatible.

#### `hyperlight_guest_bin` (~10 `flatbuffer_wrappers` imports)

| File | Imports |
|------|---------|
| `host_comm.rs` | `function_call::FunctionCall`, `function_types::{ParameterValue, ReturnType, ReturnValue}`, `guest_error::ErrorCode`, `util::get_flatbuffer_result` |
| `lib.rs` | `guest_error::ErrorCode` |
| `guest_logger.rs` | `guest_log_level::LogLevel` |
| `memory.rs` | `guest_error::ErrorCode` |
| `guest_function/definition.rs` | `function_call::FunctionCall`, `function_types::{ParameterType, ReturnType}`, `guest_error::ErrorCode`, `util::get_flatbuffer_result` |

All updated to `func::` / `guest::` paths. Also `no_std`.

#### `hyperlight_guest_capi` (9 `flatbuffer_wrappers` imports)

| File | Imports |
|------|---------|
| `flatbuffer.rs` | `util::get_flatbuffer_result` → `func::serialize::get_flatbuffer_result` |
| `dispatch.rs` | `function_call::FunctionCall`, `function_types::{ParameterType, ReturnType}`, `guest_error::ErrorCode` |
| `types/parameter.rs` | `function_types::{ParameterType, ParameterValue}` |
| `types/function_call.rs` | `function_call::FunctionCall`, `function_types::{ParameterValue, ReturnType}` |
| `error.rs` | `function_types::FunctionCallResult`, `guest_error::{ErrorCode, GuestError}` |

Also `no_std`.

#### `hyperlight_guest_tracing` (2 `flatbuffer_wrappers` imports)

| File | Imports |
|------|---------|
| `visitor.rs` | `guest_trace_data::EventKeyValue` → `guest::trace::EventKeyValue` |
| `state.rs` | `guest_trace_data::{GuestEvent, EventsEncoder, ...}` → `guest::trace::*` |

#### Test guests (5 `flatbuffer_wrappers` imports)

| File | Imports |
|------|---------|
| `simpleguest/src/main.rs` | `function_call::{FunctionCall, FunctionCallType}`, `function_types::{ParameterValue, ReturnType, ReturnValue}`, `guest_error::ErrorCode`, `guest_log_level::LogLevel`, `util::get_flatbuffer_result` |

#### Total: ~85 `flatbuffer_wrappers` import sites across all crates

All imports are updated directly to canonical `func::` / `guest::` paths in this PR.

### `no_std` considerations

- `CommonError` uses `alloc::string::String` (for `TypeConversion::got`), which is available
  under `no_std` with `extern crate alloc` (already present)
- `thiserror` is already configured with `default-features = false` in `Cargo.toml`
- No `std`-only types are introduced

### Validation

```bash
just fmt-apply
just clippy debug
just clippy release
just guests
just test-like-ci
just test-like-ci release
```

Verify that `hyperlight_guest` (which is `no_std`) still compiles.

---

## Future: Remove deprecated shims (separate release)

After at least a couple of cargo releases with the deprecated shims in place:

**`hyperlight_host` shims:**
- Remove `pub mod hypervisor { ... }` shim from `lib.rs`
- Remove `pub mod func { ... }` shim from `lib.rs`
- Remove `pub use sandbox::is_hypervisor_present;` from `lib.rs`
- Remove `pub mod uninitialized { ... }` shim from `sandbox/mod.rs`
- Remove `pub mod initialized_multi_use { ... }` shim from `sandbox/mod.rs`

This is a breaking change (major version bump).

---

## Summary

| PR | Scope | Breaking? |
|----|-------|-----------|
| 1  | Builder errors: `InternalError`, `CreateSandboxError`, `EvolveError`, `RegisterHostFunctionError` + migrate builder methods | Yes (builder API) |
| 2  | Runtime errors: `CallError`, `SnapshotError`, `RestoreError` + migrate runtime methods + remove `HyperlightError` from public API | Yes (runtime API) |
| 3  | Module restructure: rename `hypervisor/` → `vm/`, file renames, deprecated re-export shims for old paths | No (deprecated shims) |
| 4  | Delete `HyperlightError` entirely (replace with per-module errors), tighten visibility (shims kept) | No (internal only) |
| 5  | `hyperlight_common`: replace `anyhow` with typed `CommonError`, restructure `flatbuffer_wrappers/` → `func/` + `guest/`, update all downstream imports | No (internal only) |
| Future | Remove deprecated shims from PR 3 (after a couple of releases) | Yes (removes old paths) |
