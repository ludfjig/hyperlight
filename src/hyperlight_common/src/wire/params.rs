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

use alloc::vec::Vec;

use serde::{Deserialize, Serialize};

/// Whether a call targets a guest or host function.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[repr(u8)]
pub enum FunctionCallType {
    Guest = 0,
    Host = 1,
}

/// The declared return type of a function call.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[cfg_attr(feature = "fuzzing", derive(arbitrary::Arbitrary))]
#[repr(u8)]
pub enum ReturnType {
    #[default]
    Int = 0,
    UInt = 1,
    Long = 2,
    ULong = 3,
    Float = 4,
    Double = 5,
    Bool = 6,
    String = 7,
    VecBytes = 8,
    Void = 9,
}

/// Static type tag for a parameter. Mirrors [`Param`] without payloads.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[repr(u8)]
pub enum ParameterType {
    Int = 0,
    UInt = 1,
    Long = 2,
    ULong = 3,
    Float = 4,
    Double = 5,
    Bool = 6,
    String = 7,
    VecBytes = 8,
}

/// A single parameter to a function call.
///
/// `String` and `VecBytes` borrow from the wire buffer on decode.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum Param<'a> {
    Int(i32),
    UInt(u32),
    Long(i64),
    ULong(u64),
    Float(f32),
    Double(f64),
    Bool(bool),
    #[serde(borrow)]
    String(&'a str),
    #[serde(borrow, with = "serde_bytes")]
    VecBytes(&'a [u8]),
}

impl Param<'_> {
    pub fn type_tag(&self) -> ParameterType {
        match self {
            Param::Int(_) => ParameterType::Int,
            Param::UInt(_) => ParameterType::UInt,
            Param::Long(_) => ParameterType::Long,
            Param::ULong(_) => ParameterType::ULong,
            Param::Float(_) => ParameterType::Float,
            Param::Double(_) => ParameterType::Double,
            Param::Bool(_) => ParameterType::Bool,
            Param::String(_) => ParameterType::String,
            Param::VecBytes(_) => ParameterType::VecBytes,
        }
    }
}

/// A serialized function call. Name and string/byte parameters borrow
/// directly from the wire buffer on decode.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FunctionCall<'a> {
    #[serde(borrow)]
    pub name: &'a str,
    pub call_type: FunctionCallType,
    pub return_type: ReturnType,
    #[serde(borrow)]
    pub params: Vec<Param<'a>>,
}

impl<'a> FunctionCall<'a> {
    pub fn new(
        name: &'a str,
        call_type: FunctionCallType,
        return_type: ReturnType,
        params: Vec<Param<'a>>,
    ) -> Self {
        Self {
            name,
            call_type,
            return_type,
            params,
        }
    }
}

/// Wire-compatible counterpart of [`FunctionCall`] that serializes a
/// user-facing parameter tuple directly into a postcard frame, without
/// first collecting the parameters into a `Vec<Param<'a>>`.
///
/// `T` is consumed during serialization, so this struct uses interior
/// mutability under serde's `&self` API. Each value may be serialized
/// exactly once.
pub struct FunctionCallWith<'n, T> {
    pub name: &'n str,
    pub call_type: FunctionCallType,
    pub return_type: ReturnType,
    pub args: core::cell::Cell<Option<T>>,
    pub len: usize,
}

impl<'n, T> FunctionCallWith<'n, T> {
    pub fn new(
        name: &'n str,
        call_type: FunctionCallType,
        return_type: ReturnType,
        args: T,
        len: usize,
    ) -> Self {
        Self {
            name,
            call_type,
            return_type,
            args: core::cell::Cell::new(Some(args)),
            len,
        }
    }
}

impl<'n, 'a, T> serde::Serialize for FunctionCallWith<'n, T>
where
    T: crate::func::IntoParameters<'a>,
{
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeStruct;
        let mut st = s.serialize_struct("FunctionCall", 4)?;
        st.serialize_field("name", self.name)?;
        st.serialize_field("call_type", &self.call_type)?;
        st.serialize_field("return_type", &self.return_type)?;
        // Inline the params field as a sequence of known length, calling
        // each tuple element's `into_param` lazily so no `Vec<Param>` is
        // ever materialized.
        st.serialize_field(
            "params",
            &ParamsSeq {
                args: &self.args,
                len: self.len,
            },
        )?;
        st.end()
    }
}

/// Serializer adapter that emits the params field as a sequence of
/// known length without materializing a `Vec<Param>`. The args tuple
/// is stored in a `Cell<Option<_>>` and consumed by `take()` on the
/// single call to `serialize`. This relies on postcard's contract that
/// each serde field is serialized exactly once; if a future serializer
/// retries the field on error, the second attempt yields
/// `"args serialized twice"`.
struct ParamsSeq<'a, T> {
    args: &'a core::cell::Cell<Option<T>>,
    len: usize,
}

impl<'cell, 'a, T> serde::Serialize for ParamsSeq<'cell, T>
where
    T: crate::func::IntoParameters<'a>,
{
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        use serde::ser::{Error, SerializeSeq};
        let args = self
            .args
            .take()
            .ok_or_else(|| S::Error::custom("FunctionCallWith: args serialized twice"))?;
        let mut seq = s.serialize_seq(Some(self.len))?;
        args.serialize_each(&mut seq)?;
        seq.end()
    }
}
