/*
Copyright 2025 The Hyperlight Authors.

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

use alloc::ffi::CString;
use alloc::string::String;
use alloc::vec::Vec;
use core::ffi::{CStr, c_char};

use hyperlight_common::wire::{Param, ParameterType};
use hyperlight_guest::error::Result;

use crate::types::FfiVec;

/// Owned form of a parameter value used inside the C API to keep the
/// backing storage alive while a borrowed [`Param`] is constructed
/// for downstream calls.
pub enum OwnedParam {
    Int(i32),
    UInt(u32),
    Long(i64),
    ULong(u64),
    Float(f32),
    Double(f64),
    Bool(bool),
    String(String),
    VecBytes(Vec<u8>),
}

impl OwnedParam {
    /// Borrow this owned parameter as a wire [`Param`].
    pub fn as_param(&self) -> Param<'_> {
        match self {
            OwnedParam::Int(v) => Param::Int(*v),
            OwnedParam::UInt(v) => Param::UInt(*v),
            OwnedParam::Long(v) => Param::Long(*v),
            OwnedParam::ULong(v) => Param::ULong(*v),
            OwnedParam::Float(v) => Param::Float(*v),
            OwnedParam::Double(v) => Param::Double(*v),
            OwnedParam::Bool(v) => Param::Bool(*v),
            OwnedParam::String(s) => Param::String(s.as_str()),
            OwnedParam::VecBytes(v) => Param::VecBytes(v.as_slice()),
        }
    }
}

/// A union of the value stored in a Param, used for FFI.
/// On it's own, this union has no way to know which value type is stored
/// which is why it's used in conjunction with `ParameterType` in `FfiParameter`.
#[repr(C)]
#[derive(Copy, Clone)]
#[allow(non_camel_case_types, non_snake_case)]
pub union FfiParameterValue {
    pub Int: i32,
    pub UInt: u32,
    pub Long: i64,
    pub ULong: u64,
    pub Float: f32,
    pub Double: f64,
    pub Bool: bool,
    pub String: *mut c_char,
    pub VecBytes: FfiVec,
}

/// An owned FFI version of a wire [`Param`].
#[repr(C)]
#[allow(non_camel_case_types)]
pub struct FfiParameter {
    tag: ParameterType,
    value: FfiParameterValue,
}

impl FfiParameter {
    /// Returns a new `FfiParameter` by copying a wire [`Param`] into owned
    /// FFI storage.
    pub fn from_param(value: Param<'_>) -> Result<Self> {
        let (tag, union) = match value {
            Param::Int(v) => (ParameterType::Int, FfiParameterValue { Int: v }),
            Param::UInt(v) => (ParameterType::UInt, FfiParameterValue { UInt: v }),
            Param::Long(v) => (ParameterType::Long, FfiParameterValue { Long: v }),
            Param::ULong(v) => (ParameterType::ULong, FfiParameterValue { ULong: v }),
            Param::Float(v) => (ParameterType::Float, FfiParameterValue { Float: v }),
            Param::Double(v) => (ParameterType::Double, FfiParameterValue { Double: v }),
            Param::Bool(v) => (ParameterType::Bool, FfiParameterValue { Bool: v }),
            Param::String(s) => {
                let c_str = CString::new(s).expect("Unable to make CString from &str");
                let leaked = c_str.into_raw();
                (ParameterType::String, FfiParameterValue { String: leaked })
            }
            Param::VecBytes(b) => {
                let leaked = unsafe { FfiVec::from_vec(b.to_vec()) };
                (
                    ParameterType::VecBytes,
                    FfiParameterValue { VecBytes: leaked },
                )
            }
        };
        Ok(FfiParameter { tag, value: union })
    }

    /// Copies self into a new [`OwnedParam`].
    /// # Safety
    /// `self` must be an unmodified version of what `from_param` returned.
    pub unsafe fn copy_to_owned_param(&self) -> OwnedParam {
        match self.tag {
            ParameterType::Int => OwnedParam::Int(unsafe { self.value.Int }),
            ParameterType::UInt => OwnedParam::UInt(unsafe { self.value.UInt }),
            ParameterType::Long => OwnedParam::Long(unsafe { self.value.Long }),
            ParameterType::ULong => OwnedParam::ULong(unsafe { self.value.ULong }),
            ParameterType::Float => OwnedParam::Float(unsafe { self.value.Float }),
            ParameterType::Double => OwnedParam::Double(unsafe { self.value.Double }),
            ParameterType::Bool => OwnedParam::Bool(unsafe { self.value.Bool }),
            ParameterType::String => OwnedParam::String(
                unsafe { CStr::from_ptr(self.value.String) }
                    .to_string_lossy()
                    .into_owned(),
            ),
            ParameterType::VecBytes => {
                OwnedParam::VecBytes(unsafe { self.value.VecBytes.copy_to_vec() })
            }
        }
    }
}

impl Drop for FfiParameter {
    fn drop(&mut self) {
        match self.tag {
            ParameterType::String => unsafe {
                drop(CString::from_raw(self.value.String));
            },
            ParameterType::VecBytes => unsafe {
                drop(self.value.VecBytes.into_vec());
            },
            _ => {}
        }
    }
}
