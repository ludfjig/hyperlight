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

use hyperlight_common::flatbuffer_wrappers::function_types::{ParameterType, ParameterValue};
use tracing::{Span, instrument};

use super::utils::for_each_tuple;
use crate::HyperlightError::{ParameterValueConversionFailure, UnexpectedNoOfArguments};
use crate::{Result, log_then_return};

// // We can then implement these traits for each type that Hyperlight supports as a parameter or return type
// macro_rules! for_each_param_type {
//     ($macro:ident) => {
//         $macro!(String, String);
//         $macro!(i32, Int);
//         $macro!(u32, UInt);
//         $macro!(i64, Long);
//         $macro!(u64, ULong);
//         $macro!(f32, Float);
//         $macro!(f64, Double);
//         $macro!(bool, Bool);
//         $macro!(Vec<u8>, VecBytes);
//     };
// }

// macro_rules! impl_supported_param_type {
//     ($type:ty, $enum:ident) => {
//         impl SupportedParameterType for $type {
//             const TYPE: ParameterType = ParameterType::$enum;

//             fn into_value(self) -> ParameterValue {
//                 ParameterValue::$enum(self)
//             }

//             fn from_value(value: ParameterValue) -> Result<Self> {
//                 match value {
//                     ParameterValue::$enum(i) => Ok(i),
//                     other => {
//                         log_then_return!(ParameterValueConversionFailure(
//                             other.clone(),
//                             stringify!($type)
//                         ));
//                     }
//                 }
//             }
//         }
//     };
// }

// for_each_param_type!(impl_supported_param_type);

/// This is a marker trait that is used to indicate that a type is a
/// valid Hyperlight parameter type.
///
/// For each parameter type Hyperlight supports in host functions, we
/// provide an implementation for `SupportedParameterType`
pub trait SupportedParameterType<'a>: Sized + Clone + Send + Sync {
    /// The underlying Hyperlight parameter type representing this `SupportedParameterType`
    const TYPE: ParameterType;

    /// Get the underling Hyperlight parameter value representing this
    /// `SupportedParameterType`
    fn into_value(self) -> ParameterValue<'a>;
    /// Get the actual inner value of this `SupportedParameterType`
    fn from_value(value: ParameterValue<'a>) -> Result<Self>;
}

impl<'a> SupportedParameterType<'a> for &'a str {
    const TYPE: ParameterType = ParameterType::String;
    fn into_value(self) -> ParameterValue<'a> {
        ParameterValue::String(self)
    }
    fn from_value(value: ParameterValue<'a>) -> Result<Self> {
        match value {
            ParameterValue::String(i) => Ok(i),
            _ => {
                return Err(ParameterValueConversionFailure("String"));
            }
        }
    }
}

impl<'a> SupportedParameterType<'a> for &'a [u8] {
    const TYPE: ParameterType = ParameterType::VecBytes;
    fn into_value(self) -> ParameterValue<'a> {
        ParameterValue::VecBytes(self)
    }
    fn from_value(value: ParameterValue<'a>) -> Result<Self> {
        match value {
            ParameterValue::VecBytes(i) => Ok(i),
            _ => {
                return Err(ParameterValueConversionFailure("Vec<u8>"));
            }
        }
    }
}

impl<'a> SupportedParameterType<'a> for i32 {
    const TYPE: ParameterType = ParameterType::Int;
    fn into_value(self) -> ParameterValue<'a> {
        ParameterValue::Int(self)
    }
    fn from_value(value: ParameterValue<'a>) -> Result<Self> {
        match value {
            ParameterValue::Int(i) => Ok(i),
            _ => {
                return Err(ParameterValueConversionFailure("i32"));
            }
        }
    }
}

impl<'a> SupportedParameterType<'a> for u32 {
    const TYPE: ParameterType = ParameterType::UInt;
    fn into_value(self) -> ParameterValue<'a> {
        ParameterValue::UInt(self)
    }
    fn from_value(value: ParameterValue<'a>) -> Result<Self> {
        match value {
            ParameterValue::UInt(i) => Ok(i),
            _ => {
                return Err(ParameterValueConversionFailure("u32"));
            }
        }
    }
}

impl<'a> SupportedParameterType<'a> for i64 {
    const TYPE: ParameterType = ParameterType::Long;
    fn into_value(self) -> ParameterValue<'a> {
        ParameterValue::Long(self)
    }
    fn from_value(value: ParameterValue<'a>) -> Result<Self> {
        match value {
            ParameterValue::Long(i) => Ok(i),
            _ => {
                return Err(ParameterValueConversionFailure("i64"));
            }
        }
    }
}

impl<'a> SupportedParameterType<'a> for u64 {
    const TYPE: ParameterType = ParameterType::ULong;
    fn into_value(self) -> ParameterValue<'a> {
        ParameterValue::ULong(self)
    }
    fn from_value(value: ParameterValue<'a>) -> Result<Self> {
        match value {
            ParameterValue::ULong(i) => Ok(i),
            _ => {
                return Err(ParameterValueConversionFailure("u64"));
            }
        }
    }
}

impl<'a> SupportedParameterType<'a> for f32 {
    const TYPE: ParameterType = ParameterType::Float;
    fn into_value(self) -> ParameterValue<'a> {
        ParameterValue::Float(self)
    }
    fn from_value(value: ParameterValue<'a>) -> Result<Self> {
        match value {
            ParameterValue::Float(i) => Ok(i),
            _ => {
                return Err(ParameterValueConversionFailure("f32"));
            }
        }
    }
}

impl<'a> SupportedParameterType<'a> for f64 {
    const TYPE: ParameterType = ParameterType::Double;
    fn into_value(self) -> ParameterValue<'a> {
        ParameterValue::Double(self)
    }
    fn from_value(value: ParameterValue<'a>) -> Result<Self> {
        match value {
            ParameterValue::Double(i) => Ok(i),
            _ => {
                return Err(ParameterValueConversionFailure("f64"));
            }
        }
    }
}

impl<'a> SupportedParameterType<'a> for bool {
    const TYPE: ParameterType = ParameterType::Bool;
    fn into_value(self) -> ParameterValue<'a> {
        ParameterValue::Bool(self)
    }
    fn from_value(value: ParameterValue<'a>) -> Result<Self> {
        match value {
            ParameterValue::Bool(i) => Ok(i),
            _ => {
                return Err(ParameterValueConversionFailure("bool"));
            }
        }
    }
}

/// A trait to describe the tuple of parameters that a host function can take.
pub trait ParameterTuple<'a>: Sized + Clone + Send + Sync {
    /// The number of parameters in the tuple
    const SIZE: usize;

    /// The underlying Hyperlight parameter types representing this tuple of `SupportedParameterType`
    const TYPE: &'static [ParameterType];

    /// Get the underling Hyperlight parameter value representing this
    /// `SupportedParameterType`
    fn into_value(self) -> Vec<ParameterValue<'a>>;

    /// Get the actual inner value of this `SupportedParameterType`
    fn from_value(value: Vec<ParameterValue<'a>>) -> Result<Self>;
}

impl<'a, T> ParameterTuple<'a> for T
where
    T: SupportedParameterType<'a>,
{
    const SIZE: usize = 1;
    const TYPE: &'static [ParameterType] = &[T::TYPE];

    fn into_value(self) -> Vec<ParameterValue<'a>> {
        vec![self.into_value()]
    }

    fn from_value(value: Vec<ParameterValue<'a>>) -> Result<Self> {
        match <[ParameterValue<'a>; 1]>::try_from(value) {
            Ok([val]) => T::from_value(val),
            Err(value) => {
                return Err(crate::HyperlightError::UnexpectedNoOfArguments(
                    value.len(),
                    1,
                ));
            }
        }
    }
}

macro_rules! impl_param_tuple {
    ([$N:expr] ($($name:ident: $param:ident),*)) => {
        impl<'a, $($param),*> ParameterTuple<'a> for ($($param,)*)
        where
            $($param: SupportedParameterType<'a>,)*
        {
            const SIZE: usize = $N;

            const TYPE: &'static [ParameterType] = &[
                $($param::TYPE),*
            ];

            fn into_value(self) -> Vec<ParameterValue<'a>> {
                let ($($name,)*) = self;
                vec![$(SupportedParameterType::into_value($name)),*]
            }

            fn from_value(value: Vec<ParameterValue<'a>>) -> Result<Self> {
                match <[ParameterValue<'a>; $N]>::try_from(value) {
                    Ok([$($name,)*]) => Ok(($($param::from_value($name)?,)*)),
                    Err(value) => { return Err(crate::HyperlightError::UnexpectedNoOfArguments(value.len(), $N)); }
                }
            }
        }
    };
}

for_each_tuple!(impl_param_tuple);
