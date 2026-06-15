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
use alloc::string::{String, ToString};
use alloc::vec::Vec;

use hyperlight_common::outb::OutBAction;
use hyperlight_common::wire::{
    self, ErrorCode, FunctionCall, FunctionCallResult, FunctionCallType, GuestLogData, LogLevel,
    Param, ReturnType, ReturnValue,
};
use tracing::instrument;

use super::handle::GuestHandle;
use crate::error::{HyperlightGuestError, Result};
use crate::exit::out32;

impl GuestHandle {
    /// Get user memory region as bytes.
    #[instrument(skip_all, level = "Trace")]
    pub fn read_n_bytes_from_user_memory(&self, num: u64) -> Result<Vec<u8>> {
        let peb_ptr = self.peb().unwrap();
        let user_memory_region_ptr = unsafe { (*peb_ptr).init_data.ptr as *mut u8 };
        let user_memory_region_size = unsafe { (*peb_ptr).init_data.size };

        if num > user_memory_region_size {
            Err(HyperlightGuestError::new(
                ErrorCode::GuestError,
                format!(
                    "Requested {} bytes from user memory, but only {} bytes are available",
                    num, user_memory_region_size
                ),
            ))
        } else {
            let user_memory_region_slice =
                unsafe { core::slice::from_raw_parts(user_memory_region_ptr, num as usize) };
            let user_memory_region_bytes = user_memory_region_slice.to_vec();

            Ok(user_memory_region_bytes)
        }
    }

    /// Pop a host return value from the shared input stack and convert it
    /// to the requested type via [`SupportedReturnType::from_return`].
    #[instrument(skip_all, level = "Trace")]
    pub fn get_host_return_value<T>(&self) -> Result<T>
    where
        T: hyperlight_common::func::SupportedReturnType,
    {
        self.with_shared_input_top(|bytes| {
            let result: FunctionCallResult<'_> = wire::decode_exact(bytes).map_err(|e| {
                HyperlightGuestError::new(
                    ErrorCode::GuestError,
                    format!("Failed to decode host return value: {e:?}"),
                )
            })?;
            match result {
                FunctionCallResult::Ok(rv) => T::from_return(rv).map_err(Into::into),
                FunctionCallResult::Err(e) => Err(HyperlightGuestError {
                    kind: e.code,
                    message: e.message,
                }),
            }
        })
    }

    /// Pop a host return value from the shared input stack and return it
    /// as an [`OwnedReturn`] so the caller can store or forward it.
    pub fn get_host_return_raw(&self) -> Result<hyperlight_common::func::OwnedReturn> {
        self.with_shared_input_top(|bytes| {
            let result: FunctionCallResult<'_> = wire::decode_exact(bytes).map_err(|e| {
                HyperlightGuestError::new(
                    ErrorCode::GuestError,
                    format!("Failed to decode host return value: {e:?}"),
                )
            })?;
            match result {
                FunctionCallResult::Ok(rv) => Ok(rv_to_owned(rv)),
                FunctionCallResult::Err(e) => Err(HyperlightGuestError {
                    kind: e.code,
                    message: e.message,
                }),
            }
        })
    }

    /// Encode and push a host function call onto the shared output stack,
    /// then trigger the host via the `CallFunction` OUT port. The reply
    /// is left on the shared input stack for [`get_host_return_value`].
    #[instrument(skip_all, level = "Trace")]
    pub fn call_host_function_without_returning_result(
        &self,
        function_name: &str,
        parameters: Option<Vec<Param<'_>>>,
        return_type: ReturnType,
    ) -> Result<()> {
        let params = parameters.unwrap_or_default();
        let call = FunctionCall {
            name: function_name,
            call_type: FunctionCallType::Host,
            return_type,
            params,
        };

        let bytes = wire::encode(&call).map_err(|e| {
            HyperlightGuestError::new(
                ErrorCode::GuestError,
                format!("Failed to encode host function call: {e:?}"),
            )
        })?;
        self.push_shared_output_data(&bytes)?;

        unsafe {
            out32(OutBAction::CallFunction as u16, 0);
        }

        Ok(())
    }

    /// Call a host function and decode the reply as `T`.
    #[instrument(skip_all, level = "Info")]
    pub fn call_host_function<T>(
        &self,
        function_name: &str,
        parameters: Option<Vec<Param<'_>>>,
        return_type: ReturnType,
    ) -> Result<T>
    where
        T: hyperlight_common::func::SupportedReturnType,
    {
        self.call_host_function_without_returning_result(function_name, parameters, return_type)?;
        self.get_host_return_value::<T>()
    }

    /// Encode-in-place variant of [`Self::call_host_function`] that
    /// serializes a typed parameter tuple straight into the shared
    /// output buffer, skipping the `Vec<Param<'_>>` and the
    /// intermediate encoded `Vec<u8>`.
    #[instrument(skip_all, level = "Info")]
    pub fn call_host_function_with<'a, T, A>(
        &self,
        function_name: &str,
        args: A,
        return_type: ReturnType,
    ) -> Result<T>
    where
        T: hyperlight_common::func::SupportedReturnType,
        A: hyperlight_common::func::IntoParameters<'a>,
    {
        use hyperlight_common::wire::FunctionCallWith;
        let fc = FunctionCallWith::new(
            function_name,
            FunctionCallType::Host,
            return_type,
            args,
            A::LEN,
        );
        self.push_shared_output_with(|buf| {
            let written = wire::encode_into(&fc, buf).map_err(|e| {
                HyperlightGuestError::new(
                    ErrorCode::GuestError,
                    format!("Failed to encode host function call: {e:?}"),
                )
            })?;
            Ok(written.len())
        })?;

        unsafe {
            out32(OutBAction::CallFunction as u16, 0);
        }

        self.get_host_return_value::<T>()
    }

    /// Log a message with the specified log level, source, caller, source file, and line number.
    pub fn log_message(
        &self,
        log_level: LogLevel,
        message: &str,
        source: &str,
        caller: &str,
        source_file: &str,
        line: u32,
    ) {
        // Closure to send log message to host
        let _send_to_host = || {
            let guest_log_data = GuestLogData {
                message: message.to_string(),
                source: source.to_string(),
                level: log_level,
                caller: caller.to_string(),
                source_file: source_file.to_string(),
                line,
            };

            let bytes = wire::encode(&guest_log_data).expect("encode GuestLogData");

            self.push_shared_output_data(&bytes)
                .expect("Unable to push log data to shared output data");

            unsafe {
                out32(OutBAction::Log as u16, 0);
            }
        };

        #[cfg(all(feature = "trace_guest", target_arch = "x86_64"))]
        if hyperlight_guest_tracing::is_trace_enabled() {
            // If the "trace_guest" feature is enabled and tracing is initialized, log using tracing
            tracing::trace!(
                event = message,
                level = ?log_level,
                code.filepath = source,
                caller = caller,
                source_file = source_file,
                code.lineno = line,
            );
        } else {
            _send_to_host();
        }
        #[cfg(not(all(feature = "trace_guest", target_arch = "x86_64")))]
        {
            _send_to_host();
        }
    }
}

fn rv_to_owned(rv: ReturnValue<'_>) -> hyperlight_common::func::OwnedReturn {
    use hyperlight_common::func::OwnedReturn;
    match rv {
        ReturnValue::Int(v) => OwnedReturn::Int(v),
        ReturnValue::UInt(v) => OwnedReturn::UInt(v),
        ReturnValue::Long(v) => OwnedReturn::Long(v),
        ReturnValue::ULong(v) => OwnedReturn::ULong(v),
        ReturnValue::Float(v) => OwnedReturn::Float(v),
        ReturnValue::Double(v) => OwnedReturn::Double(v),
        ReturnValue::Bool(v) => OwnedReturn::Bool(v),
        ReturnValue::Void => OwnedReturn::Void,
        ReturnValue::String(s) => OwnedReturn::String(String::from(s)),
        ReturnValue::VecBytes(b) => OwnedReturn::VecBytes(b.to_vec()),
    }
}
