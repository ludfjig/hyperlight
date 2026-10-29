// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 The Hyperlight Authors.

use alloc::string::{String, ToString};
use alloc::vec::Vec;

use anyhow::{Result, anyhow, bail};
pub use bytes::Bytes;
use serde::{Deserialize, Serialize};
pub use smallvec::{SmallVec, smallvec};
#[cfg(feature = "tracing")]
use tracing::{Span, instrument};

use super::codec::{ExternalValueSink, ExternalValueSource, try_chunks_len};
use super::guest_error::GuestError;
use super::wire::{WireResult, WireValue, decode_framed, encode_framed};

pub struct FunctionCallResult(core::result::Result<ReturnValue, GuestError>);

impl FunctionCallResult {
    /// Encode control data and collect a byte return as an external value.
    pub fn encode<'a, 'b, S>(
        &'a self,
        buffer: &'b mut Vec<u8>,
        external_values: &mut S,
    ) -> Result<&'b [u8]>
    where
        S: ExternalValueSink<'a> + ?Sized,
    {
        let result = match &self.0 {
            Ok(value) => {
                if let ReturnValue::ByteChunks(v) = value {
                    let length = try_chunks_len(v)
                        .ok_or_else(|| anyhow!("External ByteChunks length overflow"))?;
                    u64::try_from(length)
                        .map_err(|_| anyhow!("External ByteChunks length does not fit in u64"))?;
                    external_values.push_chunks(v)?;
                }
                WireResult::Ok(WireValue::from_return(value))
            }
            Err(error) => WireResult::Err {
                code: error.code.into(),
                message: &error.message,
            },
        };
        encode_framed(&result, buffer)
    }

    pub fn new(value: core::result::Result<ReturnValue, GuestError>) -> Self {
        FunctionCallResult(value)
    }

    pub fn into_inner(self) -> core::result::Result<ReturnValue, GuestError> {
        self.0
    }

    /// Decode control data and consume an external byte return.
    pub fn decode<S>(value: &[u8], external_values: &mut S) -> Result<Self>
    where
        S: ExternalValueSource + ?Sized,
    {
        let result = match decode_framed(value)? {
            WireResult::Ok(value) => Ok(decode_return_value(value, external_values)?),
            WireResult::Err { code, message } => {
                Err(GuestError::new(code.into(), message.to_string()))
            }
        };

        external_values.finish()?;
        Ok(FunctionCallResult(result))
    }
}

/// Supported parameter types with values for function calling.
#[cfg_attr(feature = "fuzzing", derive(arbitrary::Arbitrary))]
#[derive(Debug, Clone, PartialEq)]
pub enum ParameterValue {
    /// i32
    Int(i32),
    /// u32
    UInt(u32),
    /// i64
    Long(i64),
    /// i64
    ULong(u64),
    /// f32
    Float(f32),
    /// f64
    Double(f64),
    /// String
    String(String),
    /// bool
    Bool(bool),
    /// One byte value split into chunks.
    ///
    /// The host receives one chunk. The guest receives one chunk per transport
    /// buffer the value spans, without copying.
    ByteChunks(
        #[cfg_attr(feature = "fuzzing", arbitrary(with = arbitrary_byte_chunks))] Vec<Bytes>,
    ),
}

#[cfg(feature = "fuzzing")]
fn arbitrary_byte_chunks(input: &mut arbitrary::Unstructured<'_>) -> arbitrary::Result<Vec<Bytes>> {
    <Vec<u8> as arbitrary::Arbitrary>::arbitrary(input).map(|value| alloc::vec![Bytes::from(value)])
}

/// Decoded call parameters. Calls with up to four parameters do not allocate.
pub type Parameters = SmallVec<[ParameterValue; 4]>;

impl ParameterValue {
    /// Borrow this value as a [`ParameterValueRef`].
    pub fn as_ref(&self) -> ParameterValueRef<'_> {
        match self {
            Self::Int(v) => ParameterValueRef::Int(*v),
            Self::UInt(v) => ParameterValueRef::UInt(*v),
            Self::Long(v) => ParameterValueRef::Long(*v),
            Self::ULong(v) => ParameterValueRef::ULong(*v),
            Self::Float(v) => ParameterValueRef::Float(*v),
            Self::Double(v) => ParameterValueRef::Double(*v),
            Self::String(v) => ParameterValueRef::String(v),
            Self::Bool(v) => ParameterValueRef::Bool(*v),
            Self::ByteChunks(v) => ParameterValueRef::ByteChunks(v),
        }
    }
}

/// A borrowed [`ParameterValue`], used to encode a function call without
/// owning its arguments.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum ParameterValueRef<'a> {
    /// i32
    Int(i32),
    /// u32
    UInt(u32),
    /// i64
    Long(i64),
    /// u64
    ULong(u64),
    /// f32
    Float(f32),
    /// f64
    Double(f64),
    /// String
    String(&'a str),
    /// bool
    Bool(bool),
    /// Bytes in one slice
    ByteSlice(&'a [u8]),
    /// Bytes in chunks
    ByteChunks(&'a [Bytes]),
}

/// Supported parameter types for function calling.
#[derive(Debug, Clone, PartialEq, Eq)]
#[repr(C)]
pub enum ParameterType {
    /// i32
    Int,
    /// u32
    UInt,
    /// i64
    Long,
    /// u64
    ULong,
    /// f32
    Float,
    /// f64
    Double,
    /// String
    String,
    /// bool
    Bool,
    /// Bytes
    ByteChunks,
}

/// Supported return types with values from function calling.
#[derive(Debug, Clone, PartialEq)]
pub enum ReturnValue {
    /// i32
    Int(i32),
    /// u32
    UInt(u32),
    /// i64
    Long(i64),
    /// u64
    ULong(u64),
    /// f32
    Float(f32),
    /// f64
    Double(f64),
    /// String
    String(String),
    /// bool
    Bool(bool),
    /// ()
    Void(()),
    /// One byte value split into chunks.
    ByteChunks(Vec<Bytes>),
}

/// Supported return types from function calling. Variant order is the wire tag.
#[cfg_attr(feature = "fuzzing", derive(arbitrary::Arbitrary))]
#[derive(Debug, Copy, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[repr(C)]
pub enum ReturnType {
    /// i32
    #[default]
    Int,
    /// u32
    UInt,
    /// i64
    Long,
    /// u64
    ULong,
    /// f32
    Float,
    /// f64
    Double,
    /// String
    String,
    /// bool
    Bool,
    /// ()
    Void,
    /// Bytes
    ByteChunks,
}

/// Take one external byte value and check its length.
fn decode_external_bytes<S>(length: u64, externals: &mut S) -> Result<Vec<Bytes>>
where
    S: ExternalValueSource + ?Sized,
{
    let length = usize::try_from(length)
        .map_err(|_| anyhow!("External byte length {length} does not fit in usize"))?;
    let value = externals.take_chunks(length)?;
    let actual = try_chunks_len(&value);

    if actual != Some(length) {
        bail!("External byte length mismatch: declared {length}, received {actual:?}");
    }
    Ok(value)
}

pub(super) fn decode_parameter_value<S>(
    value: WireValue<'_>,
    externals: &mut S,
) -> Result<ParameterValue>
where
    S: ExternalValueSource + ?Sized,
{
    Ok(match value {
        WireValue::Int(v) => ParameterValue::Int(v),
        WireValue::UInt(v) => ParameterValue::UInt(v),
        WireValue::Long(v) => ParameterValue::Long(v),
        WireValue::ULong(v) => ParameterValue::ULong(v),
        WireValue::Float(v) => ParameterValue::Float(v),
        WireValue::Double(v) => ParameterValue::Double(v),
        WireValue::String(v) => ParameterValue::String(v.to_string()),
        WireValue::Bool(v) => ParameterValue::Bool(v),
        WireValue::Void => bail!("Void is not a valid parameter value"),
        WireValue::Bytes { length } => {
            ParameterValue::ByteChunks(decode_external_bytes(length, externals)?)
        }
    })
}

fn decode_return_value<S>(value: WireValue<'_>, externals: &mut S) -> Result<ReturnValue>
where
    S: ExternalValueSource + ?Sized,
{
    Ok(match value {
        WireValue::Int(v) => ReturnValue::Int(v),
        WireValue::UInt(v) => ReturnValue::UInt(v),
        WireValue::Long(v) => ReturnValue::Long(v),
        WireValue::ULong(v) => ReturnValue::ULong(v),
        WireValue::Float(v) => ReturnValue::Float(v),
        WireValue::Double(v) => ReturnValue::Double(v),
        WireValue::String(v) => ReturnValue::String(v.to_string()),
        WireValue::Bool(v) => ReturnValue::Bool(v),
        WireValue::Void => ReturnValue::Void(()),
        WireValue::Bytes { length } => {
            ReturnValue::ByteChunks(decode_external_bytes(length, externals)?)
        }
    })
}

impl From<&ParameterValue> for ParameterType {
    #[cfg_attr(feature = "tracing", instrument(skip_all, parent = Span::current(), level= "Trace"))]
    fn from(value: &ParameterValue) -> Self {
        match *value {
            ParameterValue::Int(_) => ParameterType::Int,
            ParameterValue::UInt(_) => ParameterType::UInt,
            ParameterValue::Long(_) => ParameterType::Long,
            ParameterValue::ULong(_) => ParameterType::ULong,
            ParameterValue::Float(_) => ParameterType::Float,
            ParameterValue::Double(_) => ParameterType::Double,
            ParameterValue::String(_) => ParameterType::String,
            ParameterValue::Bool(_) => ParameterType::Bool,
            ParameterValue::ByteChunks(_) => ParameterType::ByteChunks,
        }
    }
}

#[cfg(test)]
mod tests {
    use alloc::collections::VecDeque;
    use alloc::vec;

    use super::super::guest_error::ErrorCode;
    use super::super::wire::{WireResult, WireValue, decode_framed};
    use super::*;

    #[derive(Default)]
    struct TestExternalValues {
        values: VecDeque<Vec<Bytes>>,
    }

    impl<'a> ExternalValueSink<'a> for TestExternalValues {
        fn push_bytes(&mut self, value: &'a [u8]) -> Result<()> {
            self.values.push_back(vec![Bytes::copy_from_slice(value)]);
            Ok(())
        }

        fn push_chunks(&mut self, value: &'a [Bytes]) -> Result<()> {
            self.values.push_back(value.to_vec());
            Ok(())
        }
    }

    impl ExternalValueSource for TestExternalValues {
        fn take_chunks(&mut self, _length: usize) -> Result<Vec<Bytes>> {
            self.values
                .pop_front()
                .ok_or_else(|| anyhow!("Missing external value"))
        }

        fn finish(&mut self) -> Result<()> {
            if self.values.is_empty() {
                Ok(())
            } else {
                bail!("Unused external values")
            }
        }
    }

    fn round_trip(result: core::result::Result<ReturnValue, GuestError>) -> FunctionCallResult {
        let mut buffer = Vec::new();
        let mut external_values = TestExternalValues::default();
        let encoded = FunctionCallResult::new(result)
            .encode(&mut buffer, &mut external_values)
            .unwrap();
        FunctionCallResult::decode(encoded, &mut external_values).unwrap()
    }

    #[test]
    fn encode_success_result() {
        let result = round_trip(Ok(ReturnValue::Int(42))).into_inner().unwrap();
        assert_eq!(result, ReturnValue::Int(42));
    }

    #[test]
    fn encode_error_result() {
        let error = GuestError::new(
            ErrorCode::GuestFunctionNotFound,
            "Function not found".to_string(),
        );
        let decoded = round_trip(Err(error.clone())).into_inner().unwrap_err();
        assert_eq!(decoded.code, error.code);
        assert_eq!(decoded.message, error.message);
    }

    #[test]
    fn byte_returns_round_trip_as_external_values() {
        for expected in [
            vec![Bytes::from(vec![0xa5; 4096])],
            vec![
                Bytes::from_static(b"chunk one"),
                Bytes::from_static(b" and two"),
            ],
            Vec::new(),
        ] {
            let mut buffer = Vec::new();
            let mut external_values = TestExternalValues::default();
            let value = ReturnValue::ByteChunks(expected.clone());
            let encoded = FunctionCallResult::new(Ok(value.clone()))
                .encode(&mut buffer, &mut external_values)
                .unwrap();

            assert!(encoded.len() < 4096);
            assert_eq!(
                decode_framed::<WireResult>(encoded).unwrap(),
                WireResult::Ok(WireValue::Bytes {
                    length: try_chunks_len(&expected).unwrap() as u64,
                })
            );

            let decoded = FunctionCallResult::decode(encoded, &mut external_values)
                .unwrap()
                .into_inner()
                .unwrap();
            assert_eq!(decoded, value);
            assert!(external_values.values.is_empty());
        }
    }

    #[test]
    fn external_return_decoder_rejects_invalid_values() {
        let mut buffer = Vec::new();
        let mut encoded_values = TestExternalValues::default();
        let encoded =
            FunctionCallResult::new(Ok(ReturnValue::ByteChunks(vec![Bytes::from_static(
                b"123",
            )])))
            .encode(&mut buffer, &mut encoded_values)
            .unwrap();

        let mut missing = TestExternalValues::default();
        assert!(FunctionCallResult::decode(encoded, &mut missing).is_err());

        let mut wrong_length = TestExternalValues {
            values: [vec![Bytes::from_static(b"12")]].into(),
        };
        assert!(FunctionCallResult::decode(encoded, &mut wrong_length).is_err());
    }

    #[test]
    fn external_result_decoder_rejects_unused_values() {
        let mut buffer = Vec::new();
        let mut external_values = TestExternalValues::default();
        let encoded = FunctionCallResult::new(Ok(ReturnValue::Int(42)))
            .encode(&mut buffer, &mut external_values)
            .unwrap();
        assert!(external_values.values.is_empty());

        external_values.values.push_back(Vec::new());
        assert!(FunctionCallResult::decode(encoded, &mut external_values).is_err());
    }
}
