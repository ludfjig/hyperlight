// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 The Hyperlight Authors.

use alloc::vec::Vec;

use anyhow::Result;
use bytes::Bytes;

/// Receives external byte values while their control data is encoded.
///
/// Values are delivered in their logical order without flattening chunked
/// values. The value lifetime allows a sink to keep references until the
/// control buffer has been written, without copying payload bytes.
pub trait ExternalValueSink<'a> {
    /// Receive one contiguous byte value.
    fn push_bytes(&mut self, value: &'a [u8]) -> Result<()>;

    /// Receive one logical byte sequence represented as chunks.
    fn push_chunks(&mut self, value: &'a [Bytes]) -> Result<()>;
}

/// Supplies complete external byte values while control data is decoded.
///
/// [`RecvChain`](crate::virtq::RecvChain) copies each value out of shared
/// memory into one chunk. [`Segments`](crate::virtq::Segments) shares the
/// existing transport buffers.
///
/// Implementations must validate `length` against available input and the
/// resource budget before allocating.
pub trait ExternalValueSource {
    /// Take the next external value as chunks.
    fn take_chunks(&mut self, length: usize) -> Result<Vec<Bytes>>;

    /// Finish decoding and reject any unused external values.
    fn finish(&mut self) -> Result<()>;
}

/// Total length of `chunks`, or `None` if it overflows `usize`.
pub(crate) fn try_chunks_len(chunks: &[Bytes]) -> Option<usize> {
    chunks
        .iter()
        .try_fold(0usize, |length, chunk| length.checked_add(chunk.len()))
}
