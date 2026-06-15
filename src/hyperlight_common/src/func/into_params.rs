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

use serde::ser::SerializeSeq;

use super::param_type::{Bytes, ParameterTuple, Str, SupportedParameterType};
use super::utils::for_each_tuple;
use crate::wire::Param;

/// A user-facing value convertible into a wire [`Param`]. The
/// `Carrier` associated type ties the value to its
/// [`SupportedParameterType`] so callers do not have to spell out
/// marker types at call sites.
pub trait IntoParam<'a>: Sized {
    /// The carrier type whose `Borrowed<'a>` matches `Self`.
    type Carrier: SupportedParameterType;
    /// Build the on-wire `Param` from this value.
    fn into_param(self) -> Param<'a>;
}

impl<'a> IntoParam<'a> for &'a str {
    type Carrier = Str;
    fn into_param(self) -> Param<'a> {
        Param::String(self)
    }
}

impl<'a> IntoParam<'a> for &'a [u8] {
    type Carrier = Bytes;
    fn into_param(self) -> Param<'a> {
        Param::VecBytes(self)
    }
}

macro_rules! impl_into_param_primitive {
    ($t:ty, $variant:ident) => {
        impl<'a> IntoParam<'a> for $t {
            type Carrier = $t;
            fn into_param(self) -> Param<'a> {
                Param::$variant(self)
            }
        }
    };
}

impl_into_param_primitive!(i32, Int);
impl_into_param_primitive!(u32, UInt);
impl_into_param_primitive!(i64, Long);
impl_into_param_primitive!(u64, ULong);
impl_into_param_primitive!(f32, Float);
impl_into_param_primitive!(f64, Double);
impl_into_param_primitive!(bool, Bool);

/// A user-facing tuple of values convertible to a postcard sequence
/// of [`Param`]s. The `Carrier` associated type recovers the
/// [`ParameterTuple`] shape for signature matching.
pub trait IntoParameters<'a>: Sized {
    /// The carrier tuple shape (uses [`Str`]/[`Bytes`] markers for
    /// borrowed slots).
    type Carrier: ParameterTuple;
    /// Compile-time arity of the parameter tuple, used to emit the
    /// postcard sequence length.
    const LEN: usize;
    /// Serialize each parameter directly into a postcard sequence
    /// without materializing a `Vec<Param<'a>>`.
    fn serialize_each<S: SerializeSeq>(self, seq: &mut S) -> Result<(), S::Error>;
}

impl<'a, T: IntoParam<'a>> IntoParameters<'a> for T {
    type Carrier = T::Carrier;
    const LEN: usize = 1;
    fn serialize_each<S: SerializeSeq>(self, seq: &mut S) -> Result<(), S::Error> {
        seq.serialize_element(&self.into_param())
    }
}

macro_rules! impl_into_parameters {
    ([$N:expr] ($($name:ident: $P:ident),*)) => {
        impl<'a, $($P: IntoParam<'a>),*> IntoParameters<'a> for ($($P,)*) {
            type Carrier = ($($P::Carrier,)*);
            const LEN: usize = $N;
            #[allow(unused_variables, unused_mut)]
            fn serialize_each<S: SerializeSeq>(self, seq: &mut S) -> Result<(), S::Error> {
                let ($($name,)*) = self;
                $(seq.serialize_element(&$name.into_param())?;)*
                Ok(())
            }
        }
    };
}

for_each_tuple!(impl_into_parameters);
