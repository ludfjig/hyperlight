// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 The Hyperlight Authors.

use alloc::vec::Vec;

use anyhow::Result;
use serde::{Deserialize, Serialize};

use super::guest_log_level::LogLevel;
use super::wire::{decode_framed, encode_framed};

/// A guest log record. Strings borrow from the sender's storage when encoded
/// and from the control data when decoded. Field order is the wire order.
#[derive(Eq, PartialEq, Debug, Clone, Copy, Serialize, Deserialize)]
#[allow(missing_docs)]
pub struct GuestLogData<'a> {
    pub message: &'a str,
    pub source: &'a str,
    pub level: LogLevel,
    pub caller: &'a str,
    pub source_file: &'a str,
    pub line: u32,
}

impl<'a> GuestLogData<'a> {
    /// Encode size-prefixed control data into `buffer`.
    pub fn encode<'b>(&self, buffer: &'b mut Vec<u8>) -> Result<&'b [u8]> {
        encode_framed(self, buffer)
    }

    /// Decode size-prefixed control data.
    pub fn decode(value: &'a [u8]) -> Result<Self> {
        decode_framed(value)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::transport::SIZE_PREFIX_LEN;

    fn sample() -> GuestLogData<'static> {
        GuestLogData {
            message: "message",
            source: "source",
            level: LogLevel::Warning,
            caller: "caller",
            source_file: "file.rs",
            line: 42,
        }
    }

    #[test]
    fn round_trips_and_borrows() {
        let log = sample();
        let mut buffer = Vec::new();
        let encoded = log.encode(&mut buffer).unwrap();
        let decoded = GuestLogData::decode(encoded).unwrap();

        assert_eq!(decoded, log);
        assert!(encoded.as_ptr_range().contains(&decoded.message.as_ptr()));
    }

    #[test]
    fn rejects_unknown_level() {
        let mut buffer = Vec::new();
        let mut encoded = sample().encode(&mut buffer).unwrap().to_vec();
        let level = SIZE_PREFIX_LEN + 1 + "message".len() + 1 + "source".len();
        assert_eq!(encoded[level], LogLevel::Warning as u8);
        encoded[level] = LogLevel::None as u8 + 1;

        assert!(GuestLogData::decode(&encoded).is_err());
    }

    #[test]
    fn rejects_trailing_bytes() {
        let mut buffer = Vec::new();
        let mut encoded = sample().encode(&mut buffer).unwrap().to_vec();
        encoded.push(0);
        let length = (encoded.len() - SIZE_PREFIX_LEN) as u32;
        encoded[..SIZE_PREFIX_LEN].copy_from_slice(&length.to_le_bytes());

        assert!(GuestLogData::decode(&encoded).is_err());
    }
}
