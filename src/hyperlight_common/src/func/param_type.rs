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

use alloc::vec;
use alloc::vec::Vec;
use core::marker::PhantomData;

use super::error::Error;
use super::utils::for_each_tuple;
use crate::wire::{Param, ParameterType};

/// A type usable as a guest or host function parameter.
///
/// The trait is implemented on a sized "carrier" type that determines
/// the user-facing form via the [`Borrowed`] associated type. For
/// primitives the carrier is the value type itself ([`i32`], [`bool`],
/// etc.). For zero-copy string and byte slice parameters the carriers
/// are the marker types [`Str`] and [`Bytes`] whose `Borrowed<'a>`
/// resolves to `&'a str` and `&'a [u8]` respectively.
///
/// [`Borrowed`]: SupportedParameterType::Borrowed
pub trait SupportedParameterType: Sized + 'static {
    /// The form of this parameter received by user code.
    type Borrowed<'a>;
    /// Static tag for signature matching and dispatch.
    const TYPE: ParameterType;
    /// Build the on-wire [`Param`] from a borrowed value.
    fn into_param<'a>(b: Self::Borrowed<'a>) -> Param<'a>;
    /// Extract the user-facing borrowed value from a wire [`Param`].
    fn from_param<'a>(p: Param<'a>) -> Result<Self::Borrowed<'a>, Error>;
}

/// Marker carrier whose user-facing form is `&'a str`. Use this in
/// the parameter list of `register_host_function` and friends in
/// place of `String` to receive a borrowed slice into the wire buffer.
#[derive(Debug, Default, Clone, Copy)]
pub struct Str(PhantomData<()>);

/// Marker carrier whose user-facing form is `&'a [u8]`. Use this in
/// place of `Vec<u8>` to receive a borrowed slice into the wire buffer.
#[derive(Debug, Default, Clone, Copy)]
pub struct Bytes(PhantomData<()>);

impl SupportedParameterType for Str {
    type Borrowed<'a> = &'a str;
    const TYPE: ParameterType = ParameterType::String;
    fn into_param<'a>(b: Self::Borrowed<'a>) -> Param<'a> {
        Param::String(b)
    }
    fn from_param<'a>(p: Param<'a>) -> Result<Self::Borrowed<'a>, Error> {
        match p {
            Param::String(s) => Ok(s),
            other => Err(Error::ParameterValueConversionFailure(
                other.type_tag(),
                "&str",
            )),
        }
    }
}

impl SupportedParameterType for Bytes {
    type Borrowed<'a> = &'a [u8];
    const TYPE: ParameterType = ParameterType::VecBytes;
    fn into_param<'a>(b: Self::Borrowed<'a>) -> Param<'a> {
        Param::VecBytes(b)
    }
    fn from_param<'a>(p: Param<'a>) -> Result<Self::Borrowed<'a>, Error> {
        match p {
            Param::VecBytes(b) => Ok(b),
            other => Err(Error::ParameterValueConversionFailure(
                other.type_tag(),
                "&[u8]",
            )),
        }
    }
}

macro_rules! impl_param_primitive {
    ($t:ty, $variant:ident, $label:literal) => {
        impl SupportedParameterType for $t {
            type Borrowed<'a> = $t;
            const TYPE: ParameterType = ParameterType::$variant;
            fn into_param<'a>(b: Self::Borrowed<'a>) -> Param<'a> {
                Param::$variant(b)
            }
            fn from_param<'a>(p: Param<'a>) -> Result<Self::Borrowed<'a>, Error> {
                match p {
                    Param::$variant(v) => Ok(v),
                    other => Err(Error::ParameterValueConversionFailure(
                        other.type_tag(),
                        $label,
                    )),
                }
            }
        }
    };
}

impl_param_primitive!(i32, Int, "i32");
impl_param_primitive!(u32, UInt, "u32");
impl_param_primitive!(i64, Long, "i64");
impl_param_primitive!(u64, ULong, "u64");
impl_param_primitive!(f32, Float, "f32");
impl_param_primitive!(f64, Double, "f64");
impl_param_primitive!(bool, Bool, "bool");

/// A tuple of parameters a function can take. The shape is described
/// by a tuple of carrier types; borrows propagate through to a tuple
/// of borrowed user-facing types.
pub trait ParameterTuple: Sized + 'static {
    /// The form of the parameter tuple received by user code.
    type Borrowed<'a>;
    /// Number of parameters in the tuple.
    const SIZE: usize;
    /// Static type tags for the parameter slots.
    const TYPE: &'static [ParameterType];
    /// Serialize a borrowed tuple into a `Vec<Param<'a>>`.
    fn into_params<'a>(b: Self::Borrowed<'a>) -> Vec<Param<'a>>;
    /// Deserialize from a `Vec<Param<'a>>` into the borrowed tuple.
    fn from_params<'a>(p: Vec<Param<'a>>) -> Result<Self::Borrowed<'a>, Error>;
}

impl<T: SupportedParameterType> ParameterTuple for T {
    type Borrowed<'a> = T::Borrowed<'a>;
    const SIZE: usize = 1;
    const TYPE: &'static [ParameterType] = &[T::TYPE];
    fn into_params<'a>(b: Self::Borrowed<'a>) -> Vec<Param<'a>> {
        vec![T::into_param(b)]
    }
    fn from_params<'a>(p: Vec<Param<'a>>) -> Result<Self::Borrowed<'a>, Error> {
        match <[Param<'a>; 1]>::try_from(p) {
            Ok([v]) => T::from_param(v),
            Err(v) => Err(Error::UnexpectedNoOfArguments(v.len(), 1)),
        }
    }
}

macro_rules! impl_param_tuple {
    ([$N:expr] ($($name:ident: $P:ident),*)) => {
        impl<$($P: SupportedParameterType),*> ParameterTuple for ($($P,)*) {
            type Borrowed<'a> = ($($P::Borrowed<'a>,)*);
            const SIZE: usize = $N;
            const TYPE: &'static [ParameterType] = &[$($P::TYPE),*];
            #[allow(unused_variables)]
            fn into_params<'a>(b: Self::Borrowed<'a>) -> Vec<Param<'a>> {
                let ($($name,)*) = b;
                vec![$($P::into_param($name)),*]
            }
            fn from_params<'a>(p: Vec<Param<'a>>) -> Result<Self::Borrowed<'a>, Error> {
                match <[Param<'a>; $N]>::try_from(p) {
                    Ok([$($name,)*]) => Ok(($($P::from_param($name)?,)*)),
                    Err(v) => Err(Error::UnexpectedNoOfArguments(v.len(), $N)),
                }
            }
        }
    };
}

for_each_tuple!(impl_param_tuple);
