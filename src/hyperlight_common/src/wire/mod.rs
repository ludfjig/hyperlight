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

//! Postcard-based wire format for host/guest function calls.
//!
//! Decode-time string and byte payloads borrow from the input buffer
//! via `#[serde(borrow)]`, eliminating per-parameter allocations on
//! the receive path.

use alloc::vec::Vec;

use serde::{Deserialize, Serialize};

mod host_funcs;
mod log;
mod params;
mod result;

pub use host_funcs::{HostFunctionDefinition, HostFunctionDetails};
pub use log::{GuestLogData, LogLevel};
pub use params::{
    FunctionCall, FunctionCallType, FunctionCallWith, Param, ParameterType, ReturnType,
};
pub use result::{ErrorCode, FunctionCallResult, GuestError, ReturnValue};

/// Decode a slice as `T` borrowing from `bytes`. Returns the value
/// plus the unused remainder.
pub fn decode<'a, T>(bytes: &'a [u8]) -> Result<(T, &'a [u8]), postcard::Error>
where
    T: Deserialize<'a>,
{
    postcard::take_from_bytes::<T>(bytes)
}

/// Decode a slice as `T` borrowing from `bytes`, requiring the input
/// to be exactly the encoded size. Trailing bytes are rejected with
/// `postcard::Error::DeserializeUnexpectedEnd` so a guest cannot
/// smuggle data past a valid prefix at the host boundary.
pub fn decode_exact<'a, T>(bytes: &'a [u8]) -> Result<T, postcard::Error>
where
    T: Deserialize<'a>,
{
    let (value, rest) = postcard::take_from_bytes::<T>(bytes)?;
    if !rest.is_empty() {
        return Err(postcard::Error::DeserializeBadEncoding);
    }
    Ok(value)
}

/// Encode `value` to a fresh `Vec<u8>`. Prefer [`encode_into`] when
/// writing into shared memory.
pub fn encode<T: Serialize + ?Sized>(value: &T) -> Result<Vec<u8>, postcard::Error> {
    postcard::to_allocvec(value)
}

/// Encode `value` into the provided slice. Returns the written subslice.
pub fn encode_into<'a, T: Serialize + ?Sized>(
    value: &T,
    out: &'a mut [u8],
) -> Result<&'a mut [u8], postcard::Error> {
    postcard::to_slice(value, out)
}

#[cfg(test)]
mod tests {
    use alloc::string::String;
    use alloc::vec;

    use super::*;

    #[test]
    fn roundtrip_function_call_borrows() {
        let name = String::from("Echo");
        let payload = vec![1u8, 2, 3, 4, 5];
        let call = FunctionCall {
            name: &name,
            call_type: FunctionCallType::Guest,
            return_type: ReturnType::VecBytes,
            params: vec![
                Param::String("hello"),
                Param::VecBytes(&payload),
                Param::Int(42),
            ],
        };

        let bytes = encode(&call).unwrap();
        let (decoded, rest) = decode::<FunctionCall<'_>>(&bytes).unwrap();
        assert!(rest.is_empty());
        assert_eq!(decoded.name, "Echo");
        if let Param::VecBytes(b) = decoded.params[1] {
            let r = bytes.as_ptr_range();
            assert!(r.start <= b.as_ptr() && b.as_ptr() < r.end);
        } else {
            panic!();
        }
    }

    #[test]
    fn roundtrip_return_value() {
        let s = String::from("result");
        let rv = ReturnValue::String(&s);
        let bytes = encode(&rv).unwrap();
        let (decoded, _) = decode::<ReturnValue<'_>>(&bytes).unwrap();
        assert!(matches!(decoded, ReturnValue::String(b) if b == "result"));
    }
}
