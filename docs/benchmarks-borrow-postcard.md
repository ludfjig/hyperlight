# `borrow-postcard` benchmark results vs `main`

Wire format moved from flatbuffers to postcard with `#[serde(borrow)]`, and
host/guest shared-memory I/O switched to closure-based push/pop APIs that
operate on borrowed slices.

## Reproduction

On `main`:

```bash
just guests release
cargo bench -p hyperlight-host --bench benchmarks -- \
  --save-baseline main \
  '^(guest_calls/(call|call_with_restore|call_with_host_function)/default$|guest_calls/different_thread$|guest_functions_with_large_parameters/|function_call_serialization/|sample_workloads/)'
```

On `borrow-postcard`:

```bash
just guests release
cargo bench -p hyperlight-host --bench benchmarks -- \
  --baseline main \
  '^(guest_calls/(call|call_with_restore|call_with_host_function)/default$|guest_calls/different_thread$|guest_functions_with_large_parameters/|function_call_serialization/|sample_workloads/)'
```

## Results

| Benchmark | `borrow-postcard` median | Change vs `main` | Significant |
|---|---:|---:|:---:|
| `guest_calls/call/default` | 12.66 µs | -15.96% | yes |
| `guest_calls/call_with_restore/default` | 21.74 µs | -15.98% | yes |
| `guest_calls/call_with_host_function/default` | 19.35 µs | -20.54% | yes |
| `guest_calls/different_thread` | 18.33 ms | -2.40% | no |
| `guest_functions_with_large_parameters/guest_call_with_large_parameters` | 475.38 ms | -2.00% | no |
| `function_call_serialization/serialize_function_call` | 15.27 ms | -19.25% | yes |
| `function_call_serialization/deserialize_function_call` | 1.40 ms | -92.73% | yes |
| `sample_workloads/24K_in_8K_out_c` | 13.07 µs | -43.03% | yes |
| `sample_workloads/24K_in_8K_out_rust` | 15.48 µs | -34.92% | yes |

## Interpretation

### Where the wins came from

* `guest_calls/call*` and `sample_workloads/24K_in_8K_out_*`: the path the
  refactor targeted. Per call, two `Vec<u8>` allocations and the flatbuffer
  builder went away. The C guest improved more in both absolute and relative
  terms because its per-call baseline is lower, so the same fixed savings is
  a larger fraction of total time.
* `function_call_serialization/deserialize_function_call`: the headline win.
  The bench feeds a `FunctionCall` with four 10 MB payloads (two
  `VecBytes`, two `String`). The old flatbuffer path materialized those as
  owned `Vec<u8>` and `String` copies (≈40 MB of memcpy). The new postcard
  path uses `#[serde(borrow)]`, so the deserialized fields alias the input
  buffer and only varint headers are walked.
* `function_call_serialization/serialize_function_call`: encode still has to
  write the 40 MB payload to the output buffer once. The savings are from
  skipping the flatbuffer builder's intermediate arena.

### Where nothing changed and why that is correct

* `guest_calls/different_thread` (~18 ms): dominated by VM unbind and rebind
  across threads, three orders of magnitude larger than the per-call
  serialization overhead.
* `guest_functions_with_large_parameters/guest_call_with_large_parameters`
  (~475 ms, 50 MB params): dominated by the physical memcpy of the payload
  through shared memory. The refactor removes the intermediate `Vec<u8>`,
  but the 100 MB of in plus out shared-memory traffic remains. The bench
  noise band (~13%) also exceeds any plausible win here.

## Caveat

`#[tracing::instrument]` on `call_guest_function` in
`src/hyperlight_guest_bin/src/guest_function/call.rs` is temporarily
disabled to work around a nested-push corruption on the in-place output
stack. See the `TODO` there and
`/memories/repo/push_shared_output_with-nested-pushes.md`. The benchmarks
above do not enable `trace_guest`, so this does not affect the numbers.
