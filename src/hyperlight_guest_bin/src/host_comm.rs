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

use alloc::string::ToString;
use alloc::vec::Vec;

use hyperlight_common::func::{OwnedReturn, SupportedReturnType};
use hyperlight_common::wire::{
    self, ErrorCode, FunctionCallResult, Param, ReturnType, ReturnValue,
};
use hyperlight_guest::error::{HyperlightGuestError, Result};

use crate::GUEST_HANDLE;

/// Low-level host call: encode the call frame, push it to shared output,
/// trigger the host via the `CallFunction` OUT port, then decode the
/// reply into `T`.
pub fn call_host_function<T>(
    function_name: &str,
    parameters: Option<Vec<Param<'_>>>,
    return_type: ReturnType,
) -> Result<T>
where
    T: SupportedReturnType,
{
    let handle = unsafe { GUEST_HANDLE };
    handle.call_host_function::<T>(function_name, parameters, return_type)
}

/// Typed convenience: serialize a tuple of values straight into the
/// shared output buffer and call the host. Avoids any
/// `Vec<Param<'_>>` or intermediate encoded `Vec<u8>` allocation.
/// `T` is inferred from the call site or supplied via turbofish.
pub fn call_host<'a, T, A>(function_name: impl AsRef<str>, args: A) -> Result<T>
where
    T: SupportedReturnType,
    A: hyperlight_common::func::IntoParameters<'a>,
{
    let handle = unsafe { GUEST_HANDLE };
    handle.call_host_function_with::<T, A>(function_name.as_ref(), args, T::TYPE)
}

pub fn call_host_function_without_returning_result(
    function_name: &str,
    parameters: Option<Vec<Param<'_>>>,
    return_type: ReturnType,
) -> Result<()> {
    let handle = unsafe { GUEST_HANDLE };
    handle.call_host_function_without_returning_result(function_name, parameters, return_type)
}

pub fn get_host_return_value_raw() -> Result<OwnedReturn> {
    let handle = unsafe { GUEST_HANDLE };
    handle.get_host_return_raw()
}

pub fn get_host_return_value<T: SupportedReturnType>() -> Result<T> {
    let handle = unsafe { GUEST_HANDLE };
    handle.get_host_return_value::<T>()
}

pub fn read_n_bytes_from_user_memory(num: u64) -> Result<Vec<u8>> {
    let handle = unsafe { GUEST_HANDLE };
    handle.read_n_bytes_from_user_memory(num)
}

/// Encode a [`ReturnValue`] into a wire `FunctionCallResult::Ok(...)` frame.
pub fn encode_return_value(rv: ReturnValue<'_>) -> Result<Vec<u8>> {
    let result: FunctionCallResult<'_> = FunctionCallResult::Ok(rv);
    wire::encode(&result).map_err(|e| {
        HyperlightGuestError::new(
            ErrorCode::GuestError,
            alloc::format!("Failed to encode guest return value: {e:?}"),
        )
    })
}

/// Encode a [`ReturnValue`] into a wire `FunctionCallResult::Ok(...)` frame
/// directly into `out`. Returns the number of bytes written. Use this from
/// custom guest function trampolines that want to avoid the intermediate
/// `Vec<u8>` produced by [`encode_return_value`].
pub fn encode_return_value_into(rv: ReturnValue<'_>, out: &mut [u8]) -> Result<usize> {
    let result: FunctionCallResult<'_> = FunctionCallResult::Ok(rv);
    let written = wire::encode_into(&result, out).map_err(|e| {
        HyperlightGuestError::new(
            ErrorCode::GuestError,
            alloc::format!("Failed to encode guest return value: {e:?}"),
        )
    })?;
    Ok(written.len())
}

/// Encode a typed return value into a wire `FunctionCallResult::Ok(...)`
/// frame, the format the host expects on the shared output buffer.
pub fn encode_return<T: SupportedReturnType>(value: T) -> Result<Vec<u8>> {
    let owned = value.into_owned();
    encode_return_value(owned.as_return_value())
}

/// In-place version of [`encode_return`]: serialize the typed return
/// value into `out` and return the number of bytes written.
pub fn encode_return_into<T: SupportedReturnType>(value: T, out: &mut [u8]) -> Result<usize> {
    let owned = value.into_owned();
    encode_return_value_into(owned.as_return_value(), out)
}

/// Print a message using the host's print function.
///
/// This function requires memory to be setup to be used. In particular, the
/// existence of the input and output memory regions.
pub fn print_output_with_host_print(params: Vec<Param<'_>>, out: &mut [u8]) -> Result<usize> {
    let handle = unsafe { GUEST_HANDLE };
    let mut iter = params.into_iter();
    let Some(Param::String(message)) = iter.next() else {
        return Err(HyperlightGuestError::new(
            ErrorCode::GuestError,
            "Wrong Parameters passed to print_output_with_host_print".to_string(),
        ));
    };

    let res = handle.call_host_function::<i32>(
        "HostPrint",
        Some(alloc::vec![Param::String(message)]),
        ReturnType::Int,
    )?;

    encode_return_into(res, out)
}

// Suppress unused warnings for items kept for path symmetry with the
// rewritten dispatch layer.
#[allow(dead_code)]
fn _ensure_imports_used() {
    let _ = OwnedReturn::Void;
}
