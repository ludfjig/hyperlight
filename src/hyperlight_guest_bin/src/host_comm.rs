// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 The Hyperlight Authors.

use alloc::string::ToString;
use alloc::vec::Vec;

use hyperlight_common::flatbuffer_wrappers::function_call::FunctionCall;
use hyperlight_common::flatbuffer_wrappers::function_types::{
    ParameterValue, ParameterValueRef, ReturnType, ReturnValue,
};
use hyperlight_common::flatbuffer_wrappers::guest_error::ErrorCode;
use hyperlight_common::func::{AsParameters, SupportedReturnType};
use hyperlight_guest::error::{HyperlightGuestError, Result};
use hyperlight_guest::transport;

use crate::GUEST_HANDLE;

/// Call a host function with typed arguments and return value.
pub fn call_host<T: SupportedReturnType>(
    function_name: impl AsRef<str>,
    args: impl AsParameters,
) -> Result<T> {
    let value = call_host_dynamic(
        function_name.as_ref(),
        args.as_parameters().as_ref(),
        T::TYPE,
    )?;
    Ok(T::from_value(value)?)
}

/// Call a host function whose signature is known only at runtime.
pub fn call_host_dynamic(
    function_name: &str,
    parameters: &[ParameterValueRef<'_>],
    return_type: ReturnType,
) -> Result<ReturnValue> {
    transport::with_ctx(|ctx| ctx.call_host_function(function_name, parameters, return_type))
}

/// Call a host function with owned parameter values.
#[deprecated(note = "use `call_host`, or `call_host_dynamic` for signatures known only at runtime")]
pub fn call_host_function<T: SupportedReturnType>(
    function_name: &str,
    parameters: Option<Vec<ParameterValue>>,
    return_type: ReturnType,
) -> Result<T> {
    let parameters: Vec<_> = parameters
        .iter()
        .flatten()
        .map(ParameterValue::as_ref)
        .collect();
    let value = call_host_dynamic(function_name, &parameters, return_type)?;
    Ok(T::from_value(value)?)
}

pub fn read_n_bytes_from_user_memory(num: u64) -> Result<Vec<u8>> {
    let handle = unsafe { GUEST_HANDLE };
    handle.read_n_bytes_from_user_memory(num)
}

/// Print a message using the host's print function.
#[deprecated(
    note = "register a `#[guest_function]` that calls `call_host(\"HostPrint\", message)`"
)]
pub fn print_output_with_host_print(function_call: FunctionCall<'_>) -> Result<ReturnValue> {
    if let Some(ParameterValue::String(message)) = function_call.parameters.first() {
        let res = call_host::<i32>("HostPrint", message.as_str())?;
        Ok(ReturnValue::Int(res))
    } else {
        Err(HyperlightGuestError::new(
            ErrorCode::GuestError,
            "Wrong Parameters passed to print_output_with_host_print".to_string(),
        ))
    }
}
