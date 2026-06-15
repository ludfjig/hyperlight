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

use alloc::string::String;
use alloc::vec::Vec;

use super::error::Error;
use crate::wire::{ReturnType, ReturnValue};

/// A type that may be returned from a guest or host function.
///
/// Returns are always owned at the API boundary because the function
/// produces them and they need to outlive their wire encoding.
pub trait SupportedReturnType: Sized + 'static {
    /// Static tag for the return slot.
    const TYPE: ReturnType;
    /// Encode this value into an owned [`OwnedReturn`] that lives long
    /// enough to be serialized.
    fn into_owned(self) -> OwnedReturn;
    /// Decode a wire return value into the owned Rust type.
    fn from_return<'a>(rv: ReturnValue<'a>) -> Result<Self, Error>;
}

/// Owned form of a return value used by the encode path. Holds the
/// backing storage that wire [`ReturnValue`] borrows from.
#[derive(Debug, Clone, PartialEq)]
pub enum OwnedReturn {
    Int(i32),
    UInt(u32),
    Long(i64),
    ULong(u64),
    Float(f32),
    Double(f64),
    Bool(bool),
    Void,
    String(String),
    VecBytes(Vec<u8>),
}

impl OwnedReturn {
    /// Borrow a [`ReturnValue`] over this owned storage.
    pub fn as_return_value(&self) -> ReturnValue<'_> {
        match self {
            OwnedReturn::Int(v) => ReturnValue::Int(*v),
            OwnedReturn::UInt(v) => ReturnValue::UInt(*v),
            OwnedReturn::Long(v) => ReturnValue::Long(*v),
            OwnedReturn::ULong(v) => ReturnValue::ULong(*v),
            OwnedReturn::Float(v) => ReturnValue::Float(*v),
            OwnedReturn::Double(v) => ReturnValue::Double(*v),
            OwnedReturn::Bool(v) => ReturnValue::Bool(*v),
            OwnedReturn::Void => ReturnValue::Void,
            OwnedReturn::String(s) => ReturnValue::String(s.as_str()),
            OwnedReturn::VecBytes(v) => ReturnValue::VecBytes(v.as_slice()),
        }
    }
}

macro_rules! impl_return_primitive {
    ($t:ty, $variant:ident, $label:literal) => {
        impl SupportedReturnType for $t {
            const TYPE: ReturnType = ReturnType::$variant;
            fn into_owned(self) -> OwnedReturn {
                OwnedReturn::$variant(self)
            }
            fn from_return<'a>(rv: ReturnValue<'a>) -> Result<Self, Error> {
                match rv {
                    ReturnValue::$variant(v) => Ok(v),
                    _ => Err(Error::ReturnValueConversionFailure(
                        variant_label(&rv),
                        $label,
                    )),
                }
            }
        }
    };
}

impl_return_primitive!(i32, Int, "i32");
impl_return_primitive!(u32, UInt, "u32");
impl_return_primitive!(i64, Long, "i64");
impl_return_primitive!(u64, ULong, "u64");
impl_return_primitive!(f32, Float, "f32");
impl_return_primitive!(f64, Double, "f64");
impl_return_primitive!(bool, Bool, "bool");

impl SupportedReturnType for () {
    const TYPE: ReturnType = ReturnType::Void;
    fn into_owned(self) -> OwnedReturn {
        OwnedReturn::Void
    }
    fn from_return<'a>(rv: ReturnValue<'a>) -> Result<Self, Error> {
        match rv {
            ReturnValue::Void => Ok(()),
            _ => Err(Error::ReturnValueConversionFailure(
                variant_label(&rv),
                "()",
            )),
        }
    }
}

impl SupportedReturnType for String {
    const TYPE: ReturnType = ReturnType::String;
    fn into_owned(self) -> OwnedReturn {
        OwnedReturn::String(self)
    }
    fn from_return<'a>(rv: ReturnValue<'a>) -> Result<Self, Error> {
        match rv {
            ReturnValue::String(s) => Ok(String::from(s)),
            _ => Err(Error::ReturnValueConversionFailure(
                variant_label(&rv),
                "String",
            )),
        }
    }
}

impl SupportedReturnType for Vec<u8> {
    const TYPE: ReturnType = ReturnType::VecBytes;
    fn into_owned(self) -> OwnedReturn {
        OwnedReturn::VecBytes(self)
    }
    fn from_return<'a>(rv: ReturnValue<'a>) -> Result<Self, Error> {
        match rv {
            ReturnValue::VecBytes(b) => Ok(b.to_vec()),
            _ => Err(Error::ReturnValueConversionFailure(
                variant_label(&rv),
                "Vec<u8>",
            )),
        }
    }
}

fn variant_label(rv: &ReturnValue<'_>) -> &'static str {
    match rv {
        ReturnValue::Int(_) => "i32",
        ReturnValue::UInt(_) => "u32",
        ReturnValue::Long(_) => "i64",
        ReturnValue::ULong(_) => "u64",
        ReturnValue::Float(_) => "f32",
        ReturnValue::Double(_) => "f64",
        ReturnValue::Bool(_) => "bool",
        ReturnValue::Void => "()",
        ReturnValue::String(_) => "String",
        ReturnValue::VecBytes(_) => "Vec<u8>",
    }
}

/// Bridge trait letting user functions return either `T` or
/// `Result<T, E>` where `T: SupportedReturnType`.
pub trait ResultType<E: core::fmt::Debug> {
    type ReturnType: SupportedReturnType;
    fn into_result(self) -> Result<Self::ReturnType, E>;
}

impl<T, E> ResultType<E> for T
where
    T: SupportedReturnType,
    E: core::fmt::Debug,
{
    type ReturnType = T;
    fn into_result(self) -> Result<Self::ReturnType, E> {
        Ok(self)
    }
}

impl<T, E> ResultType<E> for Result<T, E>
where
    T: SupportedReturnType,
    E: core::fmt::Debug,
{
    type ReturnType = T;
    fn into_result(self) -> Result<Self::ReturnType, E> {
        self
    }
}

/// Carrier for a guest function return type. Mirrors
/// [`SupportedParameterType`](super::SupportedParameterType) on the
/// parameter side. The lifetime-parameterized companion trait
/// [`Borrows`] names the actual user-facing return type at each
/// lifetime `'a`, which for borrowed carriers is a slice into the
/// input wire buffer.
///
/// Owned types serve as their own carrier; for them
/// `<T as Borrows<'a>>::Out = T` regardless of `'a`.
pub trait ReturnCarrier: 'static {
    /// Static tag for signature matching.
    const TYPE: ReturnType;
}

/// Lifetime-parameterized projection of a [`ReturnCarrier`] to its
/// user-facing return type at lifetime `'a`. Used as a trait bound on
/// the dispatcher closure to keep `'a` in the trait input position,
/// which is required for HRTB resolution.
pub trait Borrows<'a>: ReturnCarrier {
    /// The user-facing return type at lifetime `'a`.
    type Out: EncodeReturn;
}

/// In-place encode of a return value into the shared output buffer.
/// Writes a postcard-encoded [`FunctionCallResult::Ok`] frame.
///
/// [`FunctionCallResult::Ok`]: crate::wire::FunctionCallResult::Ok
pub trait EncodeReturn: Sized {
    /// Serialize `self` into `out` and return the number of bytes written.
    fn encode_into(self, out: &mut [u8]) -> Result<usize, postcard::Error>;
}

/// Marker carrier whose user-facing return form is `&'a [u8]`.
/// Use this in the return position of a guest function to receive a
/// borrowed slice that is encoded directly into the output buffer
/// with no intermediate allocation.
#[derive(Debug, Default, Clone, Copy)]
pub struct BytesRef;

/// Marker carrier whose user-facing return form is `&'a str`.
#[derive(Debug, Default, Clone, Copy)]
pub struct StrRef;

impl ReturnCarrier for BytesRef {
    const TYPE: ReturnType = ReturnType::VecBytes;
}
impl<'a> Borrows<'a> for BytesRef {
    type Out = &'a [u8];
}

impl ReturnCarrier for StrRef {
    const TYPE: ReturnType = ReturnType::String;
}
impl<'a> Borrows<'a> for StrRef {
    type Out = &'a str;
}

impl<T: SupportedReturnType> ReturnCarrier for T {
    const TYPE: ReturnType = <T as SupportedReturnType>::TYPE;
}
impl<'a, T: SupportedReturnType> Borrows<'a> for T {
    type Out = T;
}

impl EncodeReturn for &[u8] {
    fn encode_into(self, out: &mut [u8]) -> Result<usize, postcard::Error> {
        let fcr = crate::wire::FunctionCallResult::Ok(ReturnValue::VecBytes(self));
        Ok(postcard::to_slice(&fcr, out)?.len())
    }
}

impl EncodeReturn for &str {
    fn encode_into(self, out: &mut [u8]) -> Result<usize, postcard::Error> {
        let fcr = crate::wire::FunctionCallResult::Ok(ReturnValue::String(self));
        Ok(postcard::to_slice(&fcr, out)?.len())
    }
}

impl<T: SupportedReturnType> EncodeReturn for T {
    fn encode_into(self, out: &mut [u8]) -> Result<usize, postcard::Error> {
        let owned = SupportedReturnType::into_owned(self);
        let fcr = crate::wire::FunctionCallResult::Ok(owned.as_return_value());
        Ok(postcard::to_slice(&fcr, out)?.len())
    }
}
