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
use alloc::string::ToString;
use core::any::type_name;
use core::slice::from_raw_parts_mut;

use hyperlight_common::wire::ErrorCode;
use tracing::instrument;

use super::handle::GuestHandle;
use crate::error::{HyperlightGuestError, Result};

impl GuestHandle {
    /// Pops the top element from the shared input data buffer and returns it as a T
    #[instrument(skip_all, level = "Trace")]
    pub fn try_pop_shared_input_data_into<T>(&self) -> Result<T>
    where
        T: for<'a> TryFrom<&'a [u8]>,
    {
        let peb_ptr = self.peb().unwrap();
        let input_stack_size = unsafe { (*peb_ptr).input_stack.size as usize };
        let input_stack_ptr = unsafe { (*peb_ptr).input_stack.ptr as *mut u8 };

        let idb = unsafe { from_raw_parts_mut(input_stack_ptr, input_stack_size) };

        if idb.is_empty() {
            return Err(HyperlightGuestError::new(
                ErrorCode::GuestError,
                "Got a 0-size buffer in pop_shared_input_data_into".to_string(),
            ));
        }

        // get relative offset to next free address
        let stack_ptr_rel: u64 =
            u64::from_le_bytes(idb[..8].try_into().expect("Shared input buffer too small"));

        if stack_ptr_rel as usize > input_stack_size || stack_ptr_rel < 16 {
            return Err(HyperlightGuestError::new(
                ErrorCode::GuestError,
                format!(
                    "Invalid stack pointer: {} in pop_shared_input_data_into",
                    stack_ptr_rel
                ),
            ));
        }

        // go back 8 bytes and read. This is the offset to the element on top of stack
        let last_element_offset_rel = u64::from_le_bytes(
            idb[stack_ptr_rel as usize - 8..stack_ptr_rel as usize]
                .try_into()
                .expect("Invalid stack pointer in pop_shared_input_data_into"),
        );

        // The back-pointer must point at a valid element start: at or after the
        // 8-byte stack header and at or before the back-pointer slot itself. A
        // corrupt back-pointer here used to slice-panic; report a structured
        // guest error instead.
        let lerel = last_element_offset_rel as usize;
        let sprel = stack_ptr_rel as usize;
        if lerel < 8 || lerel > sprel.saturating_sub(8) {
            return Err(HyperlightGuestError::new(
                ErrorCode::GuestError,
                format!(
                    "Corrupt back-pointer {} in pop_shared_input_data_into (sp={})",
                    lerel, sprel
                ),
            ));
        }

        let buffer = &idb[lerel..];

        // convert the buffer to T
        let type_t = match T::try_from(buffer) {
            Ok(t) => Ok(t),
            Err(_e) => {
                return Err(HyperlightGuestError::new(
                    ErrorCode::GuestError,
                    format!("Unable to convert buffer to {}", type_name::<T>()),
                ));
            }
        };

        // update the stack pointer to point to the element we just popped of since that is now free
        idb[..8].copy_from_slice(&last_element_offset_rel.to_le_bytes());

        // zero out popped off buffer
        idb[last_element_offset_rel as usize..stack_ptr_rel as usize].fill(0);

        type_t
    }

    /// Pushes the given data onto the shared output data buffer.
    pub fn push_shared_output_data(&self, data: &[u8]) -> Result<()> {
        let peb_ptr = self.peb().unwrap();
        let output_stack_size = unsafe { (*peb_ptr).output_stack.size as usize };
        let output_stack_ptr = unsafe { (*peb_ptr).output_stack.ptr as *mut u8 };

        let odb = unsafe { from_raw_parts_mut(output_stack_ptr, output_stack_size) };

        if odb.is_empty() {
            return Err(HyperlightGuestError::new(
                ErrorCode::GuestError,
                "Got a 0-size buffer in push_shared_output_data".to_string(),
            ));
        }

        // get offset to next free address on the stack
        let stack_ptr_rel: u64 =
            u64::from_le_bytes(odb[..8].try_into().expect("Shared output buffer too small"));

        // check if the stack pointer is within the bounds of the buffer.
        // It can be equal to the size, but never greater
        // It can never be less than 8. An empty buffer's stack pointer is 8
        if stack_ptr_rel as usize > output_stack_size || stack_ptr_rel < 8 {
            return Err(HyperlightGuestError::new(
                ErrorCode::GuestError,
                format!(
                    "Invalid stack pointer: {} in push_shared_output_data",
                    stack_ptr_rel
                ),
            ));
        }

        // check if there is enough space in the buffer
        let size_required = data.len() + 8; // the data plus the pointer pointing to the data
        let size_available = output_stack_size - stack_ptr_rel as usize;
        if size_required > size_available {
            return Err(HyperlightGuestError::new(
                ErrorCode::GuestError,
                format!(
                    "Not enough space in shared output buffer. Required: {}, Available: {}",
                    size_required, size_available
                ),
            ));
        }

        // write the actual data
        odb[stack_ptr_rel as usize..stack_ptr_rel as usize + data.len()].copy_from_slice(data);

        // write the offset to the newly written data, to the top of the stack
        let bytes: [u8; 8] = stack_ptr_rel.to_le_bytes();
        odb[stack_ptr_rel as usize + data.len()..stack_ptr_rel as usize + data.len() + 8]
            .copy_from_slice(&bytes);

        // update stack pointer to point to next free address
        let new_stack_ptr_rel: u64 = (stack_ptr_rel as usize + data.len() + 8) as u64;
        odb[0..8].copy_from_slice(&(new_stack_ptr_rel).to_le_bytes());

        Ok(())
    }

    /// Run `f` against the topmost element of the shared input stack as a
    /// borrowed slice, then pop it.
    ///
    /// `f` may borrow into the slice. The slice is zeroed and the stack
    /// pointer advanced only after `f` returns, so the borrow is valid for
    /// the whole closure body. This is the entry point for zero-copy
    /// deserialization of postcard frames.
    #[instrument(skip_all, level = "Trace")]
    pub fn with_shared_input_top<R>(&self, f: impl FnOnce(&[u8]) -> Result<R>) -> Result<R> {
        let peb_ptr = self.peb().unwrap();
        let input_stack_size = unsafe { (*peb_ptr).input_stack.size as usize };
        let input_stack_ptr = unsafe { (*peb_ptr).input_stack.ptr as *mut u8 };

        let idb = unsafe { from_raw_parts_mut(input_stack_ptr, input_stack_size) };

        if idb.is_empty() {
            return Err(HyperlightGuestError::new(
                ErrorCode::GuestError,
                "Got a 0-size buffer in with_shared_input_top".to_string(),
            ));
        }

        let stack_ptr_rel: u64 =
            u64::from_le_bytes(idb[..8].try_into().expect("Shared input buffer too small"));

        if stack_ptr_rel as usize > input_stack_size || stack_ptr_rel < 16 {
            return Err(HyperlightGuestError::new(
                ErrorCode::GuestError,
                format!(
                    "Invalid stack pointer: {} in with_shared_input_top",
                    stack_ptr_rel
                ),
            ));
        }

        let last_element_offset_rel = u64::from_le_bytes(
            idb[stack_ptr_rel as usize - 8..stack_ptr_rel as usize]
                .try_into()
                .expect("Invalid stack pointer in with_shared_input_top"),
        );

        // The back-pointer must point at a valid element start: at or after the
        // 8-byte stack header and at or before the back-pointer slot itself. A
        // corrupt back-pointer here used to slice-panic; report a structured
        // guest error instead.
        let lerel = last_element_offset_rel as usize;
        let sprel = stack_ptr_rel as usize;
        if lerel < 8 || lerel > sprel.saturating_sub(8) {
            return Err(HyperlightGuestError::new(
                ErrorCode::GuestError,
                format!(
                    "Corrupt back-pointer {} in with_shared_input_top (sp={})",
                    lerel, sprel
                ),
            ));
        }

        // Reborrow as shared so `f` cannot mutate the input buffer.
        // The slice ends at the back-pointer offset, not at stack_ptr_rel.
        let payload: &[u8] = &idb[lerel..sprel - 8];
        let result = f(payload);

        // Pop regardless of f's success, matching try_pop_shared_input_data_into.
        idb[..8].copy_from_slice(&last_element_offset_rel.to_le_bytes());
        idb[lerel..sprel].fill(0);

        result
    }

    /// Write directly into the next free slot of the shared output stack via
    /// `writer`, then commit by updating the stack header.
    ///
    /// `writer` receives the free region (excluding the trailing back-pointer
    /// slot it reserves) and returns the number of bytes it actually wrote.
    /// On success the stack pointer is advanced by `n + 8` and the back-pointer
    /// is written. Skips the intermediate `Vec<u8>` that `push_shared_output_data`
    /// requires.
    ///
    /// # Nested pushes
    ///
    /// `writer` must not itself push to the same output stack. The stack
    /// pointer is committed only after `writer` returns, so any nested push
    /// reached from inside `writer` would see the stale pointer and clobber
    /// the in-flight payload. The single legitimate caller is the
    /// FunctionCallResult encode path, where `writer` is pure postcard
    /// serialization with no side effects on the output stack. See
    /// `src/hyperlight_guest_bin/src/guest_function/call.rs`.
    pub fn push_shared_output_with<F>(&self, writer: F) -> Result<()>
    where
        F: FnOnce(&mut [u8]) -> Result<usize>,
    {
        let peb_ptr = self.peb().unwrap();
        let output_stack_size = unsafe { (*peb_ptr).output_stack.size as usize };
        let output_stack_ptr = unsafe { (*peb_ptr).output_stack.ptr as *mut u8 };

        let odb = unsafe { from_raw_parts_mut(output_stack_ptr, output_stack_size) };

        if odb.is_empty() {
            return Err(HyperlightGuestError::new(
                ErrorCode::GuestError,
                "Got a 0-size buffer in push_shared_output_with".to_string(),
            ));
        }

        let stack_ptr_rel: u64 =
            u64::from_le_bytes(odb[..8].try_into().expect("Shared output buffer too small"));

        if stack_ptr_rel as usize > output_stack_size || stack_ptr_rel < 8 {
            return Err(HyperlightGuestError::new(
                ErrorCode::GuestError,
                format!(
                    "Invalid stack pointer: {} in push_shared_output_with",
                    stack_ptr_rel
                ),
            ));
        }

        // Reserve 8 bytes at the end of the free region for the back-pointer.
        let sp = stack_ptr_rel as usize;
        let available = output_stack_size.checked_sub(sp + 8).ok_or_else(|| {
            HyperlightGuestError::new(
                ErrorCode::GuestError,
                "No room in shared output buffer".to_string(),
            )
        })?;

        let written = {
            let free = &mut odb[sp..sp + available];
            writer(free)?
        };

        if written > available {
            return Err(HyperlightGuestError::new(
                ErrorCode::GuestError,
                format!(
                    "writer reported {} bytes but only {} were available",
                    written, available
                ),
            ));
        }

        // Write back-pointer immediately after the written payload.
        let bp_off = sp + written;
        odb[bp_off..bp_off + 8].copy_from_slice(&stack_ptr_rel.to_le_bytes());

        // Commit truncated stack pointer.
        let new_sp = (bp_off + 8) as u64;
        odb[0..8].copy_from_slice(&new_sp.to_le_bytes());

        Ok(())
    }
}
