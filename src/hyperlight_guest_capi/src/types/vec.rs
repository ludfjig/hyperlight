// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 The Hyperlight Authors.

use alloc::boxed::Box;
use alloc::vec::Vec;

/// A ffi compatible struct to represent a vector of u8s.
/// Copying/cloning this struct does not copy the underlying bytes.
#[repr(C)]
#[derive(Copy, Clone)]
pub struct FfiVec {
    data: *mut u8,
    len: usize,
}

impl FfiVec {
    /// Creates a new `FfiVec` from the given Vec<u8> without copying memory.
    /// # Safety
    /// The caller must not modify the returned `FfiVec`. Its memory is leaked.
    pub unsafe fn from_vec(v: Vec<u8>) -> Self {
        let leaked = Box::into_raw(v.into_boxed_slice());
        FfiVec {
            data: leaked as *mut u8,
            len: leaked.len(),
        }
    }
}
