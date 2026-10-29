// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 The Hyperlight Authors.

//! Host RPC encoding and decoding over virtqueue chains.

use anyhow::{Context, bail};
use hyperlight_common::flatbuffer_wrappers::ExternalValueSource;
use hyperlight_common::flatbuffer_wrappers::function_call::FunctionCall;
use hyperlight_common::flatbuffer_wrappers::function_types::FunctionCallResult;
use hyperlight_common::flatbuffer_wrappers::guest_log_data::GuestLogData;
use hyperlight_common::transport::{
    EncodedMessage, ExternalValues, MsgHeader, MsgKind, SIZE_PREFIX_LEN,
};
use hyperlight_common::virtq::{RecvChain, WritableChain};

use super::mem::HostMemOps;

/// Decode one complete host function call from a G2H request.
///
/// Control data is copied out of guest-writable scratch into `buffer`. The
/// function name borrows from `buffer`. External values are copied.
/// Unconsumed trailing bytes are rejected.
pub(crate) fn get_host_function_call<'b>(
    chain: &mut RecvChain<HostMemOps>,
    buffer: &'b mut Vec<u8>,
) -> anyhow::Result<FunctionCall<'b>> {
    let control = read_control(chain, buffer)?;
    FunctionCall::decode(control, chain)
}

/// Read and validate one complete G2H message header.
pub(crate) fn read_message_header(
    request: &mut RecvChain<HostMemOps>,
) -> anyhow::Result<MsgHeader> {
    let mut bytes = [0u8; MsgHeader::SIZE];
    request.read_exact(&mut bytes)?;

    let header = MsgHeader::from_bytes(&bytes).context("G2H message has an invalid header")?;
    if header.payload_len as usize != request.remaining() {
        bail!(
            "G2H message declares {} payload bytes, received {}",
            header.payload_len,
            request.remaining()
        );
    }

    Ok(header)
}

/// Decode a guest-function result body after its G2H header.
pub(crate) fn read_guest_function_call_result(
    request: &mut RecvChain<HostMemOps>,
    buffer: &mut Vec<u8>,
) -> anyhow::Result<FunctionCallResult> {
    let control = read_control(request, buffer)?;
    FunctionCallResult::decode(control, request)
}

/// Append a complete response when it fits the remaining writable space.
///
/// `false` leaves the writable chain unchanged.
pub(crate) fn try_write_response(
    reply: &mut WritableChain<HostMemOps>,
    cid: u32,
    result: &FunctionCallResult,
    buffer: &mut Vec<u8>,
) -> anyhow::Result<bool> {
    let mut externals = ExternalValues::new();

    let control = result.encode(buffer, &mut externals)?;
    let msg = EncodedMessage::new(MsgKind::Response, cid, control, externals)
        .context("Host function response length overflow")?;

    if msg.total_len() > reply.remaining() {
        return Ok(false);
    }

    for chunk in msg.chunks() {
        reply.write_all(chunk)?;
    }

    Ok(true)
}

/// Decode guest log data and reject trailing external bytes.
///
/// Strings borrow from `buffer`.
pub(crate) fn read_guest_log_data<'b>(
    chain: &mut RecvChain<HostMemOps>,
    buffer: &'b mut Vec<u8>,
) -> anyhow::Result<GuestLogData<'b>> {
    let control = read_control(chain, buffer)?;
    chain.finish()?;
    GuestLogData::decode(control)
}

/// Copy size-prefixed control data into `buffer` and leave external values
/// unread.
fn read_control<'b>(
    request: &mut RecvChain<HostMemOps>,
    buffer: &'b mut Vec<u8>,
) -> anyhow::Result<&'b [u8]> {
    let mut prefix = [0u8; SIZE_PREFIX_LEN];
    request.read_exact(&mut prefix)?;

    let payload_len = u32::from_le_bytes(prefix) as usize;
    if payload_len > request.remaining() {
        bail!(
            "G2H control data declares {payload_len} bytes, only {} remain",
            request.remaining()
        );
    }

    // The prefix and payload fit within the original chain length.
    let control_len = SIZE_PREFIX_LEN + payload_len;
    buffer.clear();
    buffer
        .try_reserve_exact(control_len)
        .with_context(|| format!("Failed to allocate {control_len} bytes for G2H control data"))?;
    buffer.extend_from_slice(&prefix);

    request.read_exact_uninit(&mut buffer.spare_capacity_mut()[..payload_len])?;
    // SAFETY: `read_exact_uninit` initialized the `payload_len` bytes after
    // the prefix, which fit the reserved capacity.
    unsafe { buffer.set_len(control_len) };

    Ok(buffer)
}

#[cfg(test)]
mod tests {
    use hyperlight_common::flatbuffer_wrappers::function_types::ReturnValue;
    use hyperlight_common::virtq::{BufferChainBuilder, MemOps, RingProducer, VirtqError};

    use super::*;
    use crate::mem::virtq::tests::TestCase;

    fn with_control(payload: &[u8], test: impl FnOnce(&mut RecvChain<HostMemOps>)) {
        let case = TestCase::new();

        let mut producer = RingProducer::new(case.g2h_layout, case.mem.clone());
        case.mem.write(case.g2h_pool_base, payload).unwrap();

        let chain = BufferChainBuilder::new()
            .readable(case.g2h_pool_base, payload.len() as u32)
            .build()
            .unwrap();

        producer.submit_available(&chain).unwrap();

        let mut consumer = case.g2h_consumer();
        let (mut request, reply) = consumer.poll(payload.len()).unwrap().unwrap();

        test(&mut request);
        consumer.complete(request, reply).unwrap();
    }

    #[test]
    fn read_control_returns_only_new_bytes() {
        with_control(b"\x04\0\0\0ctrl", |request| {
            let mut buffer = vec![0xff; 64];
            assert_eq!(
                read_control(request, &mut buffer).unwrap(),
                b"\x04\0\0\0ctrl"
            );
        });
    }

    #[test]
    fn read_control_rejects_truncated_prefix() {
        with_control(b"\x04\0\0", |request| {
            let error = read_control(request, &mut Vec::new()).unwrap_err();
            assert!(matches!(
                error.downcast_ref::<VirtqError>(),
                Some(VirtqError::ReceiveTooShort {
                    requested: SIZE_PREFIX_LEN,
                    remaining: 3,
                })
            ));
            assert_eq!(request.consumed(), 0);
        });
    }

    #[test]
    fn read_control_rejects_truncated_body() {
        with_control(b"\x04\0\0\0ctr", |request| {
            let error = read_control(request, &mut Vec::new()).unwrap_err();
            assert_eq!(
                error.to_string(),
                "G2H control data declares 4 bytes, only 3 remain"
            );
            assert_eq!(request.remaining(), 3);
        });
    }

    #[test]
    fn read_control_leaves_external_bytes_unread() {
        let case = TestCase::new();
        let mut producer = RingProducer::new(case.g2h_layout, case.mem.clone());
        let payload = b"\x04\0\0\0ctrlext";
        let addr = case.g2h_pool_base;
        case.mem.write(addr, payload).unwrap();

        let chain = BufferChainBuilder::new()
            .readable(addr, 2)
            .readable(addr + 2, 5)
            .readable(addr + 7, 4)
            .build()
            .unwrap();
        producer.submit_available(&chain).unwrap();

        let mut consumer = case.g2h_consumer();
        let (mut request, reply) = consumer.poll(payload.len()).unwrap().unwrap();
        assert_eq!(
            read_control(&mut request, &mut Vec::new()).unwrap(),
            b"\x04\0\0\0ctrl"
        );
        assert_eq!(request.remaining(), 3);

        let mut external = [0; 3];
        request.read_exact(&mut external).unwrap();
        assert_eq!(&external, b"ext");
        consumer.complete(request, reply).unwrap();
    }

    #[test]
    fn response_preflight_leaves_short_reply_unchanged() {
        let queue = TestCase::new();
        let mut consumer = queue.h2g_consumer();
        let (request, reply) = consumer.poll(0).unwrap().unwrap();

        let Ok(mut reply) = reply.into_writable() else {
            panic!("expected writable reply");
        };

        let result = FunctionCallResult::new(Ok(ReturnValue::UInt(17)));
        let mut builder = Vec::new();
        let mut externals = ExternalValues::new();

        let control = result.encode(&mut builder, &mut externals).unwrap();
        let message = EncodedMessage::new(MsgKind::Response, 7, control, externals).unwrap();

        let prefix = vec![0xa5; reply.capacity() - message.total_len() + 1];
        reply.write_all(&prefix).unwrap();
        let addr = queue.h2g_desc(0).addr;
        let before = queue.h2g_buffer(0, addr);

        assert!(!try_write_response(&mut reply, 7, &result, &mut Vec::new()).unwrap());
        assert_eq!(reply.written(), prefix.len());
        assert_eq!(queue.h2g_buffer(0, addr), before);

        consumer.complete(request, reply).unwrap();
    }

    #[test]
    fn response_fits_remaining_capacity_exactly() {
        let queue = TestCase::new();
        let mut consumer = queue.h2g_consumer();
        let (request, reply) = consumer.poll(0).unwrap().unwrap();
        let Ok(mut reply) = reply.into_writable() else {
            panic!("expected writable reply");
        };

        let result = FunctionCallResult::new(Ok(ReturnValue::UInt(17)));
        let mut builder = Vec::new();
        let mut externals = ExternalValues::new();

        let control = result.encode(&mut builder, &mut externals).unwrap();
        let message = EncodedMessage::new(MsgKind::Response, 7, control, externals).unwrap();

        let expected: Vec<_> = message.chunks().flatten().copied().collect();

        let prefix = vec![0xa5; reply.capacity() - expected.len()];
        reply.write_all(&prefix).unwrap();

        assert!(try_write_response(&mut reply, 7, &result, &mut Vec::new()).unwrap());
        assert_eq!(reply.remaining(), 0);

        let actual = queue.h2g_buffer(0, queue.h2g_desc(0).addr);
        assert_eq!(&actual[..prefix.len()], prefix);
        assert_eq!(&actual[prefix.len()..], expected);

        consumer.complete(request, reply).unwrap();
    }
}
