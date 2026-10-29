// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 The Hyperlight Authors.

//! Postcard control encoding for function calls and results.
//!
//! Control data is a little-endian `u32` size prefix followed by one postcard
//! value. Byte values travel as external values and the control data carries
//! only their length.

use alloc::vec::Vec;
use core::fmt;
use core::marker::PhantomData;

use anyhow::{Result, anyhow, bail};
use serde::de::{SeqAccess, Visitor};
use serde::ser::SerializeSeq;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use smallvec::SmallVec;

use super::codec::try_chunks_len;
use super::function_call::FunctionCallType;
use super::function_types::{ParameterValueRef, ReturnType, ReturnValue};
use crate::transport::SIZE_PREFIX_LEN;

/// One control value. Variant order is the wire tag.
#[derive(Debug, PartialEq, Serialize, Deserialize)]
pub(super) enum WireValue<'a> {
    Int(i32),
    UInt(u32),
    Long(i64),
    ULong(u64),
    Float(f32),
    Double(f64),
    String(&'a str),
    Bool(bool),
    Void,
    Bytes { length: u64 },
}

impl<'a> WireValue<'a> {
    /// Byte lengths must have been checked by the caller.
    pub(super) fn from_parameter(value: ParameterValueRef<'a>) -> Self {
        match value {
            ParameterValueRef::Int(v) => Self::Int(v),
            ParameterValueRef::UInt(v) => Self::UInt(v),
            ParameterValueRef::Long(v) => Self::Long(v),
            ParameterValueRef::ULong(v) => Self::ULong(v),
            ParameterValueRef::Float(v) => Self::Float(v),
            ParameterValueRef::Double(v) => Self::Double(v),
            ParameterValueRef::String(v) => Self::String(v),
            ParameterValueRef::Bool(v) => Self::Bool(v),
            ParameterValueRef::ByteSlice(v) => Self::Bytes {
                length: v.len() as u64,
            },
            ParameterValueRef::ByteChunks(v) => Self::Bytes {
                length: try_chunks_len(v).unwrap_or_default() as u64,
            },
        }
    }

    /// Byte lengths must have been checked by the caller.
    pub(super) fn from_return(value: &'a ReturnValue) -> Self {
        match value {
            ReturnValue::Int(v) => Self::Int(*v),
            ReturnValue::UInt(v) => Self::UInt(*v),
            ReturnValue::Long(v) => Self::Long(*v),
            ReturnValue::ULong(v) => Self::ULong(*v),
            ReturnValue::Float(v) => Self::Float(*v),
            ReturnValue::Double(v) => Self::Double(*v),
            ReturnValue::String(v) => Self::String(v),
            ReturnValue::Bool(v) => Self::Bool(*v),
            ReturnValue::Void(()) => Self::Void,
            ReturnValue::ByteChunks(v) => Self::Bytes {
                length: try_chunks_len(v).unwrap_or_default() as u64,
            },
        }
    }
}

/// Serializes parameters as a sequence without collecting them first.
pub(super) struct ParamsOut<I>(pub(super) I);

impl<'a, I> Serialize for ParamsOut<I>
where
    I: Clone + ExactSizeIterator<Item = ParameterValueRef<'a>>,
{
    fn serialize<S: Serializer>(&self, serializer: S) -> core::result::Result<S::Ok, S::Error> {
        let values = self.0.clone();
        let mut seq = serializer.serialize_seq(Some(values.len()))?;
        for value in values {
            seq.serialize_element(&WireValue::from_parameter(value))?;
        }
        seq.end()
    }
}

/// Largest parameter count in a call, matching the largest supported function
/// arity. It bounds decode allocation for untrusted input.
pub(super) const MAX_PARAMS: usize = 32;

/// Inline capacity covers the common parameter counts without heap use.
pub(super) struct ParamsIn<'a>(pub(super) SmallVec<[WireValue<'a>; 8]>);

impl<'de: 'a, 'a> Deserialize<'de> for ParamsIn<'a> {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> core::result::Result<Self, D::Error> {
        struct ParamsVisitor<'a>(PhantomData<&'a ()>);

        impl<'de: 'a, 'a> Visitor<'de> for ParamsVisitor<'a> {
            type Value = ParamsIn<'a>;

            fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
                f.write_str("a parameter sequence")
            }

            fn visit_seq<A: SeqAccess<'de>>(
                self,
                mut seq: A,
            ) -> core::result::Result<Self::Value, A::Error> {
                // The length hint is untrusted, so growth follows decoded elements.
                let mut values = SmallVec::new();
                while let Some(value) = seq.next_element()? {
                    if values.len() == MAX_PARAMS {
                        return Err(serde::de::Error::custom("too many parameters"));
                    }
                    values.push(value);
                }
                Ok(ParamsIn(values))
            }
        }

        deserializer.deserialize_seq(ParamsVisitor(PhantomData))
    }
}

#[derive(Serialize)]
pub(super) struct CallOut<'n, P> {
    pub(super) name: &'n str,
    pub(super) call_type: FunctionCallType,
    pub(super) return_type: ReturnType,
    pub(super) params: P,
}

#[derive(Deserialize)]
pub(super) struct CallIn<'a> {
    pub(super) name: &'a str,
    pub(super) call_type: FunctionCallType,
    pub(super) return_type: ReturnType,
    #[serde(borrow)]
    pub(super) params: ParamsIn<'a>,
}

#[derive(Debug, PartialEq, Serialize, Deserialize)]
pub(super) enum WireResult<'a> {
    #[serde(borrow)]
    Ok(WireValue<'a>),
    Err {
        code: u64,
        message: &'a str,
    },
}

/// Write `value` as size-prefixed control data into `buffer`.
pub(super) fn encode_framed<'b, T: Serialize>(
    value: &T,
    buffer: &'b mut Vec<u8>,
) -> Result<&'b [u8]> {
    let mut out = core::mem::take(buffer);
    out.clear();
    out.extend_from_slice(&[0; SIZE_PREFIX_LEN]);

    let mut out = postcard::to_extend(value, out).map_err(|e| anyhow!("Encode failed: {e}"))?;
    let length = u32::try_from(out.len() - SIZE_PREFIX_LEN)
        .map_err(|_| anyhow!("Control data exceeds the size prefix range"))?;
    out[..SIZE_PREFIX_LEN].copy_from_slice(&length.to_le_bytes());

    *buffer = out;
    Ok(buffer.as_slice())
}

/// Read one size-prefixed postcard value and reject trailing bytes.
pub(super) fn decode_framed<'a, T: Deserialize<'a>>(value: &'a [u8]) -> Result<T> {
    let Some((prefix, body)) = value.split_first_chunk::<SIZE_PREFIX_LEN>() else {
        bail!("Control data is missing its size prefix");
    };

    let length = u32::from_le_bytes(*prefix) as usize;
    if body.len() != length {
        bail!(
            "Control data declares {length} bytes, received {}",
            body.len()
        );
    }

    let (decoded, rest) =
        postcard::take_from_bytes(body).map_err(|e| anyhow!("Decode failed: {e}"))?;
    if !rest.is_empty() {
        bail!("Control data has {} trailing bytes", rest.len());
    }
    Ok(decoded)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::flatbuffer_wrappers::function_types::ParameterValue;

    #[test]
    fn value_wire_tags_are_stable() {
        let cases = [
            (WireValue::Int(0), 0u8),
            (WireValue::UInt(0), 1),
            (WireValue::Long(0), 2),
            (WireValue::ULong(0), 3),
            (WireValue::Float(0.0), 4),
            (WireValue::Double(0.0), 5),
            (WireValue::String(""), 6),
            (WireValue::Bool(false), 7),
            (WireValue::Void, 8),
            (WireValue::Bytes { length: 0 }, 9),
        ];
        for (value, tag) in cases {
            let bytes = postcard::to_extend(&value, Vec::new()).unwrap();
            assert_eq!(bytes[0], tag, "{value:?}");
        }
    }

    #[test]
    fn decode_rejects_more_than_max_params() {
        let decode = |count: usize| {
            let params = alloc::vec![ParameterValue::Int(0); count];
            let call = CallOut {
                name: "f",
                call_type: FunctionCallType::Host,
                return_type: ReturnType::Int,
                params: ParamsOut(params.iter().map(ParameterValue::as_ref)),
            };
            let mut buffer = Vec::new();
            let encoded = encode_framed(&call, &mut buffer).unwrap().to_vec();
            decode_framed::<CallIn>(&encoded).map(|call| call.params.0.len())
        };

        assert_eq!(decode(MAX_PARAMS).unwrap(), MAX_PARAMS);
        assert!(decode(MAX_PARAMS + 1).is_err());
    }

    fn tag<T: Serialize>(value: &T) -> u8 {
        postcard::to_extend(value, Vec::new()).unwrap()[0]
    }

    #[test]
    fn type_wire_tags_are_stable() {
        let return_types = [
            ReturnType::Int,
            ReturnType::UInt,
            ReturnType::Long,
            ReturnType::ULong,
            ReturnType::Float,
            ReturnType::Double,
            ReturnType::String,
            ReturnType::Bool,
            ReturnType::Void,
            ReturnType::ByteChunks,
        ];
        for (expected, value) in return_types.iter().enumerate() {
            assert_eq!(tag(value), expected as u8, "{value:?}");
        }
        assert_eq!(tag(&FunctionCallType::Guest), 0);
        assert_eq!(tag(&FunctionCallType::Host), 1);
        assert!(postcard::from_bytes::<ReturnType>(&[return_types.len() as u8]).is_err());
    }

    #[test]
    fn framing_rejects_bad_lengths_and_trailing_bytes() {
        let mut buffer = Vec::new();
        let encoded = encode_framed(&WireValue::Int(7), &mut buffer)
            .unwrap()
            .to_vec();
        assert_eq!(
            decode_framed::<WireValue>(&encoded).unwrap(),
            WireValue::Int(7)
        );

        assert!(decode_framed::<WireValue>(&encoded[..SIZE_PREFIX_LEN - 1]).is_err());
        assert!(decode_framed::<WireValue>(&encoded[..encoded.len() - 1]).is_err());

        let mut trailing = encoded.clone();
        trailing.push(0);
        let length = (trailing.len() - SIZE_PREFIX_LEN) as u32;
        trailing[..SIZE_PREFIX_LEN].copy_from_slice(&length.to_le_bytes());
        assert!(decode_framed::<WireValue>(&trailing).is_err());
    }
}
