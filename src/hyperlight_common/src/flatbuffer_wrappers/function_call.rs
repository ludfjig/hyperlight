// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 The Hyperlight Authors.

use alloc::vec::Vec;

use anyhow::Result;
use serde::{Deserialize, Serialize};

use super::codec::{ExternalValueSink, ExternalValueSource, try_chunks_len};
use super::function_types::{ParameterValueRef, Parameters, ReturnType, decode_parameter_value};
use super::wire::{CallIn, CallOut, ParamsOut, decode_framed, encode_framed};

/// The type of function call. Variant order is the wire tag.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum FunctionCallType {
    /// The function call is to a guest function.
    Guest,
    /// The function call is to a host function.
    Host,
}

/// `Functioncall` represents a call to a function in the guest or host.
/// A call to a function in the guest or host.
///
/// The name borrows from the caller or from received control data.
/// Parameters up to [`Parameters`]'s inline capacity do not allocate.
#[derive(Debug, Clone, PartialEq)]
pub struct FunctionCall<'a> {
    /// The function name
    pub function_name: &'a str,
    /// The parameters for the function call.
    pub parameters: Parameters,
    /// The type of the function call.
    pub function_call_type: FunctionCallType,
    /// The return type of the function call
    pub expected_return_type: ReturnType,
}

impl<'a> FunctionCall<'a> {
    /// The type of the function call.
    #[deprecated(note = "read the `function_call_type` field")]
    pub fn function_call_type(&self) -> FunctionCallType {
        self.function_call_type
    }

    /// Encode a call and collect byte parameters as external values.
    pub fn encode<'p, 'b, S, I>(
        function_name: &str,
        function_call_type: FunctionCallType,
        expected_return_type: ReturnType,
        parameters: I,
        buffer: &'b mut Vec<u8>,
        external_values: &mut S,
    ) -> Result<&'b [u8]>
    where
        S: ExternalValueSink<'p> + ?Sized,
        I: IntoIterator<Item = ParameterValueRef<'p>>,
        I::IntoIter: Clone + ExactSizeIterator,
    {
        let parameters = parameters.into_iter();
        for parameter in parameters.clone() {
            match parameter {
                ParameterValueRef::ByteSlice(value) => {
                    u64::try_from(value.len()).map_err(|_| {
                        anyhow::anyhow!("External byte parameter length does not fit in u64")
                    })?;
                    external_values.push_bytes(value)?;
                }
                ParameterValueRef::ByteChunks(value) => {
                    let length = try_chunks_len(value).ok_or_else(|| {
                        anyhow::anyhow!("External ByteChunks parameter length overflow")
                    })?;
                    u64::try_from(length).map_err(|_| {
                        anyhow::anyhow!("External ByteChunks parameter length does not fit in u64")
                    })?;
                    external_values.push_chunks(value)?;
                }
                _ => {}
            }
        }

        let call = CallOut {
            name: function_name,
            call_type: function_call_type,
            return_type: expected_return_type,
            params: ParamsOut(parameters),
        };
        encode_framed(&call, buffer)
    }

    /// Decode control data and consume external byte parameters.
    ///
    /// The function name borrows from `value`.
    pub fn decode<S>(value: &'a [u8], external_values: &mut S) -> Result<Self>
    where
        S: ExternalValueSource + ?Sized,
    {
        let call: CallIn<'a> = decode_framed(value)?;
        let parameters = call
            .params
            .0
            .into_iter()
            .map(|value| decode_parameter_value(value, external_values))
            .collect::<Result<Parameters>>()?;

        external_values.finish()?;
        Ok(Self {
            function_name: call.name,
            parameters,
            function_call_type: call.call_type,
            expected_return_type: call.return_type,
        })
    }
}

#[cfg(test)]
mod tests {
    use alloc::collections::VecDeque;
    use alloc::vec;

    use super::*;
    use crate::flatbuffer_wrappers::function_types::{Bytes, ParameterValue, ReturnType};
    use crate::flatbuffer_wrappers::wire::WireValue;

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
                .ok_or_else(|| anyhow::anyhow!("Missing external value"))
        }

        fn finish(&mut self) -> Result<()> {
            if self.values.is_empty() {
                Ok(())
            } else {
                anyhow::bail!("Unused external values")
            }
        }
    }

    #[test]
    fn scalar_parameters_round_trip() -> Result<()> {
        let mut builder = Vec::new();
        let mut external_values = TestExternalValues::default();
        let test_data = FunctionCall::encode(
            "PrintTwelveArgs",
            FunctionCallType::Guest,
            ReturnType::Int,
            [
                ParameterValueRef::String("1"),
                ParameterValueRef::Int(2),
                ParameterValueRef::Long(3),
                ParameterValueRef::String("4"),
                ParameterValueRef::String("5"),
                ParameterValueRef::Bool(true),
                ParameterValueRef::Bool(false),
                ParameterValueRef::UInt(8),
                ParameterValueRef::ULong(9),
                ParameterValueRef::Int(10),
                ParameterValueRef::Float(3.123),
                ParameterValueRef::Double(0.01),
            ],
            &mut builder,
            &mut external_values,
        )?;

        let function_call = FunctionCall::decode(test_data, &mut external_values)?;
        assert_eq!(function_call.function_name, "PrintTwelveArgs");
        let parameters = function_call.parameters;
        assert_eq!(parameters.len(), 12);
        let expected_parameters = vec![
            ParameterValue::String("1".to_string()),
            ParameterValue::Int(2),
            ParameterValue::Long(3),
            ParameterValue::String("4".to_string()),
            ParameterValue::String("5".to_string()),
            ParameterValue::Bool(true),
            ParameterValue::Bool(false),
            ParameterValue::UInt(8),
            ParameterValue::ULong(9),
            ParameterValue::Int(10),
            ParameterValue::Float(3.123),
            ParameterValue::Double(0.01),
        ];
        assert_eq!(parameters.as_slice(), expected_parameters.as_slice());
        assert_eq!(function_call.function_call_type, FunctionCallType::Guest);

        Ok(())
    }

    #[test]
    fn byte_parameters_round_trip_in_external_value_order() {
        let large = vec![0xa5; 4096];
        let chunks = [
            Bytes::from_static(b"chunk one"),
            Bytes::from_static(b" and two"),
        ];
        let parameters = [
            ParameterValueRef::Int(7),
            ParameterValueRef::ByteSlice(&large),
            ParameterValueRef::String("middle"),
            ParameterValueRef::ByteChunks(&chunks),
            ParameterValueRef::ByteSlice(&[]),
            ParameterValueRef::ByteChunks(&[]),
        ];
        let mut buffer = Vec::new();
        let mut external_values = TestExternalValues::default();
        let encoded = FunctionCall::encode(
            "external_bytes",
            FunctionCallType::Host,
            ReturnType::ByteChunks,
            parameters,
            &mut buffer,
            &mut external_values,
        )
        .unwrap();

        assert!(encoded.len() < 4096);
        assert_eq!(external_values.values.len(), 4);

        let encoded_call: CallIn<'_> = decode_framed(encoded).unwrap();
        for (index, length) in [(1, 4096), (3, 17), (4, 0), (5, 0)] {
            assert_eq!(encoded_call.params.0[index], WireValue::Bytes { length });
        }

        let decoded = FunctionCall::decode(encoded, &mut external_values).unwrap();
        let bytes = |value: &ParameterValue| match value {
            ParameterValue::ByteChunks(v) => v.concat(),
            other => panic!("expected bytes, got {other:?}"),
        };
        assert_eq!(decoded.function_name, "external_bytes");
        assert_eq!(decoded.parameters[0], ParameterValue::Int(7));
        assert_eq!(bytes(&decoded.parameters[1]), large);
        assert_eq!(
            decoded.parameters[2],
            ParameterValue::String("middle".to_string())
        );
        assert_eq!(bytes(&decoded.parameters[3]), b"chunk one and two");
        assert!(bytes(&decoded.parameters[4]).is_empty());
        assert!(bytes(&decoded.parameters[5]).is_empty());
        assert_eq!(decoded.function_call_type, FunctionCallType::Host);
        assert_eq!(decoded.expected_return_type, ReturnType::ByteChunks);
        assert!(external_values.values.is_empty());
    }

    #[test]
    fn scalar_call_uses_no_external_values() {
        let mut builder = Vec::new();
        let mut external_values = TestExternalValues::default();
        let encoded = FunctionCall::encode(
            "scalars",
            FunctionCallType::Guest,
            ReturnType::Bool,
            [
                ParameterValueRef::Int(42),
                ParameterValueRef::String("value"),
            ],
            &mut builder,
            &mut external_values,
        )
        .unwrap();

        assert!(external_values.values.is_empty());
        let decoded = FunctionCall::decode(encoded, &mut external_values).unwrap();
        assert_eq!(decoded.function_name, "scalars");
    }

    #[test]
    fn external_parameter_decoder_rejects_invalid_value_sequences() {
        let mut builder = Vec::new();
        let mut encoded_values = TestExternalValues::default();
        let encoded = FunctionCall::encode(
            "external_bytes",
            FunctionCallType::Guest,
            ReturnType::Void,
            [ParameterValueRef::ByteSlice(&[1, 2, 3])],
            &mut builder,
            &mut encoded_values,
        )
        .unwrap();

        let mut missing = TestExternalValues::default();
        assert!(FunctionCall::decode(encoded, &mut missing).is_err());

        let mut wrong_length = TestExternalValues {
            values: [vec![Bytes::from_static(&[1, 2])]].into(),
        };
        assert!(FunctionCall::decode(encoded, &mut wrong_length).is_err());

        let mut extra = TestExternalValues {
            values: [vec![Bytes::from_static(&[1, 2, 3])], Vec::new()].into(),
        };
        assert!(FunctionCall::decode(encoded, &mut extra).is_err());
    }
}
