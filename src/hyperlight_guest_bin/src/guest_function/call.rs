/*
Copyright 2025  The Hyperlight Authors.

Licensed under the Apache License, Version 2.0 (the "License");
you may not use this file except in compliance with the License.
You may obtain a copy of the License at

    http://www.apache.org/licenses/LICENSE-2.0

Unless required by applicable law or agreed to in writing, software
distributed under the License is distributed on an "AS IS" BASIS,
WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
See the License for the specific language governing permissions and
limitations under the License.
*/

use alloc::format;
use alloc::vec::Vec;

use hyperlight_common::wire::{
    self, ErrorCode, FunctionCall, FunctionCallResult, FunctionCallType, GuestError, ParameterType,
};
use hyperlight_guest::bail;
use hyperlight_guest::error::{HyperlightGuestError, Result};

use crate::{GUEST_HANDLE, REGISTERED_GUEST_FUNCTIONS};

core::arch::global_asm!(
    ".weak guest_dispatch_function",
    ".set guest_dispatch_function, {}",
    sym guest_dispatch_function_default,
);

#[tracing::instrument(skip_all, parent = tracing::Span::current(), level= "Trace")]
fn guest_dispatch_function_default(function_call: FunctionCall<'_>) -> Result<Vec<u8>> {
    let name = function_call.name;
    bail!(ErrorCode::GuestFunctionNotFound => "No handler found for function call: {name:#?}");
}

// `#[tracing::instrument]` is intentionally not applied here. The macro's
// exit event would push a trace frame to the same output stack that
// `push_shared_output_with` is mid-reservation on, clobbering the encoded
// `FunctionCallResult`. See
// `src/hyperlight_guest/src/guest_handle/io.rs::push_shared_output_with`
// for the contract on nested pushes.
pub(crate) fn call_guest_function(
    function_call: FunctionCall<'_>,
    out: &mut [u8],
) -> Result<usize> {
    // Validate this is a Guest Function Call
    if function_call.call_type != FunctionCallType::Guest {
        return Err(HyperlightGuestError::new(
            ErrorCode::GuestError,
            format!(
                "Invalid function call type: {:#?}, should be Guest.",
                function_call.call_type
            ),
        ));
    }

    // Find the function definition for the function call.
    // Use &raw const to get an immutable reference to the static HashMap
    // this is to avoid the clippy warning "shared reference to mutable static"
    #[allow(clippy::deref_addrof)]
    if let Some(registered_function_definition) =
        unsafe { (*(&raw const REGISTERED_GUEST_FUNCTIONS)).get(function_call.name) }
    {
        let function_call_parameter_types: Vec<ParameterType> =
            function_call.params.iter().map(|p| p.type_tag()).collect();

        // Verify that the function call has the correct parameter types and length.
        registered_function_definition.verify_parameters(&function_call_parameter_types)?;

        (registered_function_definition.function_pointer)(function_call.params, out)
    } else {
        // The given function is not registered. The guest should implement a function called
        // guest_dispatch_function to handle this.
        //
        // The fallback path keeps the legacy `Result<Vec<u8>>` ABI so existing
        // user dispatchers (including the C API shim) continue to compile.
        // We pay one extra copy here to splat the returned bytes into `out`.

        // TODO: ideally we would define a default implementation of this with weak linkage so the guest is not required
        // to implement the function but its seems that weak linkage is an unstable feature so for now its probably better
        // to not do that.
        unsafe extern "Rust" {
            fn guest_dispatch_function(function_call: FunctionCall<'_>) -> Result<Vec<u8>>;
        }

        let bytes = unsafe { guest_dispatch_function(function_call) }?;
        if bytes.len() > out.len() {
            return Err(HyperlightGuestError::new(
                ErrorCode::GuestError,
                format!(
                    "guest_dispatch_function returned {} bytes but only {} available in output buffer",
                    bytes.len(),
                    out.len()
                ),
            ));
        }
        out[..bytes.len()].copy_from_slice(&bytes);
        Ok(bytes.len())
    }
}

pub(crate) fn internal_dispatch_function() {
    // Read the current TSC to report it to the host with the spans/events
    // This helps calculating the timestamps relative to the guest call
    #[cfg(all(feature = "trace_guest", target_arch = "x86_64"))]
    let _entered = {
        let guest_start_tsc = hyperlight_guest_tracing::invariant_tsc::read_tsc();
        // Reset the trace state for the new guest function call with the new start TSC
        // This clears any existing spans/events from previous calls ensuring a clean state
        hyperlight_guest_tracing::new_call(guest_start_tsc);

        tracing::span!(tracing::Level::INFO, "internal_dispatch_function").entered()
    };

    let handle = unsafe { GUEST_HANDLE };

    // Borrow the input frame and write the reply into the output frame
    // in a single pass. The decoded `FunctionCall<'_>` borrows directly
    // from the shared input buffer, and the guest function serializes its
    // `FunctionCallResult` postcard frame straight into the shared output
    // buffer, avoiding any intermediate `Vec<u8>` for the steady-state
    // success path.
    let push = handle.with_shared_input_top(|bytes| {
        let call: FunctionCall<'_> = wire::decode_exact(bytes).map_err(|e| {
            HyperlightGuestError::new(
                ErrorCode::GuestError,
                format!("Failed to decode guest FunctionCall: {e:?}"),
            )
        })?;
        handle.push_shared_output_with(|out| match call_guest_function(call, out) {
            Ok(n) => Ok(n),
            Err(err) => {
                let fcr: FunctionCallResult<'_> = FunctionCallResult::Err(GuestError {
                    code: err.kind,
                    message: err.message,
                });
                let written = wire::encode_into(&fcr, out).map_err(|e| {
                    HyperlightGuestError::new(
                        ErrorCode::GuestError,
                        format!("Failed to encode FunctionCallResult::Err: {e:?}"),
                    )
                })?;
                Ok(written.len())
            }
        })
    });

    if let Err(err) = push {
        // If even the in-place encode failed (e.g. decode failure or
        // output buffer too small), fall back to a heap-allocated error
        // reply so the host still sees a structured failure.
        let fcr: FunctionCallResult<'_> = FunctionCallResult::Err(GuestError {
            code: err.kind,
            message: err.message,
        });
        let data = wire::encode(&fcr).expect("encode FunctionCallResult::Err");
        handle
            .push_shared_output_data(&data)
            .expect("Failed to serialize function call result");
    }

    // All this tracing logic shall be done right before the call to `hlt` which is done after this
    // function returns
    #[cfg(all(feature = "trace_guest", target_arch = "x86_64"))]
    {
        // This span captures the internal dispatch function only, without tracing internals.
        // Close the span before flushing to ensure that the `flush` call is not included in the span
        // NOTE: This is necessary to avoid closing the span twice. Flush closes all the open
        // spans, when preparing to close a guest function call context.
        // It is not mandatory, though, but avoids a warning on the host that alerts a spans
        // that has not been opened but is being closed.
        _entered.exit();

        // Ensure that any tracing output during the call is flushed to
        // the host, if necessary.
        hyperlight_guest_tracing::flush();
    }
}
