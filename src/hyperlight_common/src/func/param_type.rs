// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 The Hyperlight Authors.

use alloc::string::String;
use alloc::vec::Vec;

use bytes::Bytes;

use super::error::Error;
use super::utils::for_each_tuple;
use crate::flatbuffer_wrappers::function_types::{
    ParameterType, ParameterValue, ParameterValueRef, Parameters,
};

/// This is a marker trait that is used to indicate that a type is a
/// valid Hyperlight parameter type.
///
/// For each parameter type Hyperlight supports in host functions, we
/// provide an implementation for `SupportedParameterType`
#[diagnostic::on_unimplemented(
    message = "`{Self}` is not a Hyperlight parameter type",
    note = "byte parameters are `Vec<Bytes>`: in the guest each chunk shares a transport buffer without copying",
    note = "joining the chunks into one buffer copies the whole value"
)]
pub trait SupportedParameterType: Sized + Clone + Send + Sync + 'static {
    /// The underlying Hyperlight parameter type representing this `SupportedParameterType`
    const TYPE: ParameterType;

    /// Get the actual inner value of this `SupportedParameterType`
    fn from_value(value: ParameterValue) -> Result<Self, Error>;
}

// We can then implement these traits for each type that Hyperlight supports as a parameter or return type
macro_rules! for_each_param_type {
    ($macro:ident) => {
        $macro!(String, String);
        $macro!(i32, Int);
        $macro!(u32, UInt);
        $macro!(i64, Long);
        $macro!(u64, ULong);
        $macro!(f32, Float);
        $macro!(f64, Double);
        $macro!(bool, Bool);
        $macro!(Vec<Bytes>, ByteChunks);
    };
}

macro_rules! impl_supported_param_type {
    ($type:ty, $enum:ident) => {
        impl SupportedParameterType for $type {
            const TYPE: ParameterType = ParameterType::$enum;

            fn from_value(value: ParameterValue) -> Result<Self, Error> {
                match value {
                    ParameterValue::$enum(i) => Ok(i),
                    other => Err(Error::ParameterValueConversionFailure(
                        other.clone(),
                        stringify!($type),
                    )),
                }
            }
        }
    };
}

for_each_param_type!(impl_supported_param_type);

/// A function argument. Arguments are borrowed and copied once into the call.
#[diagnostic::on_unimplemented(
    message = "`{Self}` is not a Hyperlight argument",
    note = "arguments are borrowed and copied once into the call, so pass `&str`, `&[u8]` or `&[Bytes]`",
    note = "cloning or building an owned value only to pass it adds a copy"
)]
pub trait AsParameter {
    /// Borrow this value as a [`ParameterValueRef`].
    fn as_parameter(&self) -> ParameterValueRef<'_>;
}

macro_rules! impl_as_parameter_copy {
    ($($type:ty => $enum:ident),*) => {$(
        impl AsParameter for $type {
            fn as_parameter(&self) -> ParameterValueRef<'_> {
                ParameterValueRef::$enum(*self)
            }
        }
    )*};
}

impl_as_parameter_copy!(i32 => Int, u32 => UInt, i64 => Long, u64 => ULong, f32 => Float, f64 => Double, bool => Bool);

impl AsParameter for &str {
    fn as_parameter(&self) -> ParameterValueRef<'_> {
        ParameterValueRef::String(self)
    }
}

impl AsParameter for &[u8] {
    fn as_parameter(&self) -> ParameterValueRef<'_> {
        ParameterValueRef::ByteSlice(self)
    }
}

impl<const N: usize> AsParameter for &[u8; N] {
    fn as_parameter(&self) -> ParameterValueRef<'_> {
        ParameterValueRef::ByteSlice(*self)
    }
}

impl AsParameter for &[Bytes] {
    fn as_parameter(&self) -> ParameterValueRef<'_> {
        ParameterValueRef::ByteChunks(self)
    }
}

/// A tuple of function arguments.
#[diagnostic::on_unimplemented(
    message = "`{Self}` is not a Hyperlight argument list",
    note = "arguments are borrowed and copied once into the call, so pass `&str`, `&[u8]` or `&[Bytes]`",
    note = "cloning or building an owned value only to pass it adds a copy"
)]
pub trait AsParameters {
    /// Borrowed arguments in order. A fixed-size array, so encoding a call
    /// does not allocate.
    type Refs<'a>: AsRef<[ParameterValueRef<'a>]>
    where
        Self: 'a;

    /// Borrow these values as [`ParameterValueRef`]s.
    fn as_parameters(&self) -> Self::Refs<'_>;
}

impl<T: AsParameter> AsParameters for T {
    type Refs<'a>
        = [ParameterValueRef<'a>; 1]
    where
        Self: 'a;

    fn as_parameters(&self) -> Self::Refs<'_> {
        [self.as_parameter()]
    }
}

macro_rules! impl_as_parameters {
    ([$N:expr] ($($name:ident: $param:ident),*)) => {
        impl<$($param: AsParameter),*> AsParameters for ($($param,)*) {
            type Refs<'a>
                = [ParameterValueRef<'a>; $N]
            where
                Self: 'a;

            #[allow(clippy::unused_unit)]
            fn as_parameters(&self) -> Self::Refs<'_> {
                let ($($name,)*) = self;
                [$($name.as_parameter()),*]
            }
        }
    };
}

for_each_tuple!(impl_as_parameters);

/// A trait to describe the tuple of parameters that a host function can take.
pub trait ParameterTuple: Sized + Clone + Send + Sync + 'static {
    /// The number of parameters in the tuple
    const SIZE: usize;

    /// The underlying Hyperlight parameter types representing this tuple of `SupportedParameterType`
    const TYPE: &[ParameterType];

    /// Get the actual inner value of this `SupportedParameterType`
    fn from_value(value: Parameters) -> Result<Self, Error>;
}

impl<T: SupportedParameterType> ParameterTuple for T {
    const SIZE: usize = 1;

    const TYPE: &[ParameterType] = &[T::TYPE];

    fn from_value(value: Parameters) -> Result<Self, Error> {
        let len = value.len();
        let mut values = value.into_iter();
        match (values.next(), values.next()) {
            (Some(val), None) => Ok(T::from_value(val)?),
            _ => Err(Error::UnexpectedNoOfArguments(len, 1)),
        }
    }
}

macro_rules! impl_param_tuple {
    ([$N:expr] ($($name:ident: $param:ident),*)) => {
        impl<$($param: SupportedParameterType),*> ParameterTuple for ($($param,)*) {
            const SIZE: usize = $N;

            const TYPE: &[ParameterType] = &[
                $($param::TYPE),*
            ];

            #[allow(unused_mut, unused_variables)]
            fn from_value(value: Parameters) -> Result<Self, Error> {
                let len = value.len();
                if len != $N {
                    return Err(Error::UnexpectedNoOfArguments(len, $N));
                }
                let mut values = value.into_iter();
                Ok(($(
                    $param::from_value(
                        values.next().ok_or(Error::UnexpectedNoOfArguments(len, $N))?,
                    )?,
                )*))
            }
        }
    };
}

for_each_tuple!(impl_param_tuple);

#[cfg(test)]
mod tests {
    use alloc::vec;

    use super::*;

    #[test]
    fn arguments_borrow() {
        let chunks = [Bytes::from_static(&[1, 2])];
        let args = ("text", [1u8, 2].as_slice(), b"ab", chunks.as_slice(), 3_i32);
        assert_eq!(
            args.as_parameters(),
            [
                ParameterValueRef::String("text"),
                ParameterValueRef::ByteSlice(&[1, 2]),
                ParameterValueRef::ByteSlice(b"ab"),
                ParameterValueRef::ByteChunks(&chunks),
                ParameterValueRef::Int(3),
            ]
        );
    }

    #[test]
    fn byte_chunks_parameter_round_trips_without_copying() {
        let chunks = vec![Bytes::from_static(b"hello"), Bytes::from_static(b" world")];
        let first_chunk = chunks[0].as_ptr();
        let value = ParameterValue::ByteChunks(chunks);
        let chunks = <Vec<Bytes> as SupportedParameterType>::from_value(value).unwrap();

        assert_eq!(chunks[0].as_ptr(), first_chunk);
        assert_eq!(chunks.concat(), b"hello world");
    }
}
