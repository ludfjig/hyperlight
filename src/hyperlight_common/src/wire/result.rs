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

use serde::de::{Error as DeError, Unexpected};
use serde::{Deserialize, Deserializer, Serialize, Serializer};

/// The return payload of a successful function call. Borrows from the
/// wire buffer on decode.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum ReturnValue<'a> {
    Int(i32),
    UInt(u32),
    Long(i64),
    ULong(u64),
    Float(f32),
    Double(f64),
    Bool(bool),
    Void,
    #[serde(borrow)]
    String(&'a str),
    #[serde(borrow, with = "serde_bytes")]
    VecBytes(&'a [u8]),
}

/// Error codes reported from the guest. Numeric values are stable
/// across the wire and across the C ABI: the manual `Serialize` and
/// `Deserialize` impls below emit/accept the explicit discriminants
/// (not the serde-default variant index), so adding or reordering
/// variants does not perturb existing values.
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
#[repr(u8)]
pub enum ErrorCode {
    NoError = 0,
    UnsupportedParameterType = 2,
    GuestFunctionNameNotProvided = 3,
    GuestFunctionNotFound = 4,
    GuestFunctionIncorrectNoOfParameters = 5,
    DispatchFunctionPointerNotSet = 6,
    OutbError = 7,
    UnknownError = 8,
    StackCheckFailed = 10,
    TooManyGuestFunctions = 11,
    FailureInDlmalloc = 12,
    MallocFailed = 13,
    GuestFunctionParameterTypeMismatch = 14,
    GuestError = 15,
    ArrayLengthParamIsMissing = 16,
    HostFunctionError = 17,
}

impl ErrorCode {
    /// Decode a numeric wire value back to a known variant. Returns
    /// `None` for any byte that is not a valid `ErrorCode`.
    pub fn from_u8(value: u8) -> Option<Self> {
        Some(match value {
            0 => Self::NoError,
            2 => Self::UnsupportedParameterType,
            3 => Self::GuestFunctionNameNotProvided,
            4 => Self::GuestFunctionNotFound,
            5 => Self::GuestFunctionIncorrectNoOfParameters,
            6 => Self::DispatchFunctionPointerNotSet,
            7 => Self::OutbError,
            8 => Self::UnknownError,
            10 => Self::StackCheckFailed,
            11 => Self::TooManyGuestFunctions,
            12 => Self::FailureInDlmalloc,
            13 => Self::MallocFailed,
            14 => Self::GuestFunctionParameterTypeMismatch,
            15 => Self::GuestError,
            16 => Self::ArrayLengthParamIsMissing,
            17 => Self::HostFunctionError,
            _ => return None,
        })
    }
}

impl Serialize for ErrorCode {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_u8(*self as u8)
    }
}

impl<'de> Deserialize<'de> for ErrorCode {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let value = u8::deserialize(deserializer)?;
        Self::from_u8(value).ok_or_else(|| {
            D::Error::invalid_value(Unexpected::Unsigned(value as u64), &"a valid ErrorCode")
        })
    }
}

/// An error reported by the guest. Errors are rare, so owning the
/// message keeps the type `'static`-friendly across the boundary.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct GuestError {
    pub code: ErrorCode,
    pub message: String,
}

/// The result of a function call as it appears on the wire.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum FunctionCallResult<'a> {
    #[serde(borrow)]
    Ok(ReturnValue<'a>),
    Err(GuestError),
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The wire byte for every variant must match the explicit
    /// `#[repr(u8)]` discriminant, regardless of declaration order.
    #[test]
    fn error_code_wire_values_match_discriminants() {
        let cases = [
            (ErrorCode::NoError, 0u8),
            (ErrorCode::UnsupportedParameterType, 2),
            (ErrorCode::GuestFunctionNameNotProvided, 3),
            (ErrorCode::GuestFunctionNotFound, 4),
            (ErrorCode::GuestFunctionIncorrectNoOfParameters, 5),
            (ErrorCode::DispatchFunctionPointerNotSet, 6),
            (ErrorCode::OutbError, 7),
            (ErrorCode::UnknownError, 8),
            (ErrorCode::StackCheckFailed, 10),
            (ErrorCode::TooManyGuestFunctions, 11),
            (ErrorCode::FailureInDlmalloc, 12),
            (ErrorCode::MallocFailed, 13),
            (ErrorCode::GuestFunctionParameterTypeMismatch, 14),
            (ErrorCode::GuestError, 15),
            (ErrorCode::ArrayLengthParamIsMissing, 16),
            (ErrorCode::HostFunctionError, 17),
        ];

        for (code, wire) in cases {
            let bytes = postcard::to_allocvec(&code).unwrap();
            assert_eq!(bytes.as_slice(), &[wire], "encode {code:?}");
            let decoded: ErrorCode = postcard::from_bytes(&bytes).unwrap();
            assert_eq!(decoded, code);
        }
    }

    #[test]
    fn error_code_rejects_unknown_byte() {
        let bytes = [9u8];
        let res: Result<ErrorCode, _> = postcard::from_bytes(&bytes);
        assert!(res.is_err());
    }
}
