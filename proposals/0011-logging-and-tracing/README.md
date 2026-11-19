# HIP 0011 - Logging and tracing

<!-- toc -->
- [Summary](#summary)
- [Terms](#terms)
- [Motivation](#motivation)
- [Proposal](#proposal)
  - [Turning it on](#turning-it-on)
  - [Output](#output)
  - [OpenTelemetry](#opentelemetry)
  - [Guest code](#guest-code)
  - [Overhead](#overhead)
  - [Migration](#migration)
- [Design](#design)
  - [Host](#host)
  - [Guest](#guest)
  - [Cargo features](#cargo-features)
  - [Hyperlight's own output](#hyperlights-own-output)
- [Reference](#reference)
  - [Sandbox level methods](#sandbox-level-methods)
  - [Fields](#fields)
  - [Measurements](#measurements)
- [Test plan](#test-plan)
- [Drawbacks](#drawbacks)
- [Alternatives](#alternatives)
<!-- /toc -->

## Summary

Guest output becomes opt-in `tracing` events that any `tracing` subscriber can
filter and print. The embedder turns it on per sandbox. OpenTelemetry export
moves to a new crate, `hyperlight-otel`. The `trace_guest` Cargo features go
away, and guest output costs nothing unless it is on.

## Terms

* Embedder: the program that uses `hyperlight-host`.
* Guest event: an event from guest code, made with a `tracing` macro such as
  `info!` or a `log` macro such as `log::info!`.
* Guest span: a `tracing` span in guest code.
* Span close event: the `tracing` event the host emits when a guest span
  closes.
* Guest output: guest events and guest spans.
* Sandbox level: the most verbose guest output a sandbox sends, set by the
  embedder per sandbox.
* Spans on: a sandbox level is set and the embedder's filter enables target
  `hyperlight_spans`. Spans off is everything else.
* Log: the `log` crate. This HIP uses "log" for nothing else.

## Motivation

* Lost output:
  * Guest `tracing` events are dropped unless the guest is built with
    `trace_guest` or enables the `tracing` crate's `log` Cargo feature.
  * In guests built with `trace_guest`, guest `tracing` events reach the host
    only as OpenTelemetry span events. A `fmt` subscriber never shows them. At
    sandbox level `TRACE`, the same holds for `log` events.
  * The host sends guest spans to `opentelemetry::global`. Each
    `opentelemetry` version has its own global. If the embedder uses another
    version, guest spans are lost with no error.
* Tied to OpenTelemetry:
  * Guest spans only reach embedders that use OpenTelemetry.
* Unclear API:
  * Guest code can use `log` or `tracing` macros. The docs do not say which
    to use.
* Bad defaults and configuration:
  * Guest events are on by default, at level `ERROR`. Each guest event costs a
    VM exit, and guest output is untrusted.
  * The host reads `RUST_LOG` to pick the guest level. Libraries should not
    read `RUST_LOG`.
  * A sandbox from a snapshot keeps the snapshot's guest level when it sets
    none.
  * Guest spans need the `trace_guest` Cargo feature on both the host and the
    guest. The host feature slows every guest call, even when nothing records
    spans. A guest built with it fails on a host without it (#1863).
* Noisy host output:
  * 15 host spans have no level, so they are at `INFO`. They include
    `Sandbox::call` and `restore`, so a plain `info` filter prints Hyperlight
    spans on every guest call.
  * 57 host functions log their returned error at `ERROR`, and 30
    `log_then_return!` sites do the same. One failure logs several times.
    Expected errors, such as a guest function that returns an error, log at
    `ERROR` too.
  * Some of these errors come from decoding guest data, so a guest can make
    the host log at `ERROR` as often as it likes.
* Hard to test:
  * Logging tests share the global `log` logger, so they run one at a time in
    a separate recipe. Two of them never run.
  * No test runs guest spans from a real guest. The test guests are built
    without the guest `trace_guest` feature.

Non-goals: metrics, memory profiling, span timestamp accuracy, and guest spans
on aarch64.

## Proposal

* [Turning it on](#turning-it-on): guest output is off by default.
* [Output](#output): guest output becomes `tracing` events.
* [OpenTelemetry](#opentelemetry): support moves to a new crate.
* [Guest code](#guest-code): use `tracing` macros.
* [Overhead](#overhead): no cost unless guest output is on.
* [Migration](#migration): the breaking changes.

[Design](#design) covers the internals, including removing the
`trace_guest` features.

### Turning it on

Guest output depends on two settings:

* The sandbox level. The embedder sets it per sandbox with the existing
  [`guest_log_level` methods](#sandbox-level-methods), which take a
  `LevelFilter`. It covers all guest output. With no sandbox level, guest
  output is off. The host does not read `RUST_LOG`.
* The filter. The embedder sets it once for the whole process, on its
  `tracing` subscriber, for example with `EnvFilter`. It applies to guest
  output like any other `tracing` event.

Guest output appears only if both settings allow it.

```rust
tracing_subscriber::fmt()
    .with_env_filter(EnvFilter::new("info"))
    .init();

let sandbox = SandboxBuilder::from_file(path)
    .guest_log_level(LevelFilter::INFO)
    .build()?;
```

Examples, with `guest_log_level(LevelFilter::DEBUG)` on the sandbox:

| `tracing` subscriber filter | Guest events | Guest spans |
|---|---|---|
| No `tracing` subscriber | Off | Off |
| `info` | `error` to `info` | `error` to `info` |
| `info,hyperlight_spans=off` | `error` to `info` | Off |
| `debug` | `error` to `debug` | `error` to `debug` |

### Output

Every `tracing` event has a target. Filters such as `EnvFilter` select events
by target and level. Hyperlight uses two fixed targets, so filters do not
depend on guest module names:

* `hyperlight_guest`: guest events, at the level the guest code gave them.
* `hyperlight_spans`: span close events, at the guest span's level, and
  `call-to-guest`.

The separate targets let an embedder turn off guest spans and keep guest
events. `hyperlight_spans` does not start with `hyperlight_guest`, because
`EnvFilter` matches targets by prefix.

While a sandbox level is set, the host creates a `tracing` span,
`call-to-guest`, around each guest call and around guest init. It covers the
whole guest execution, from the first vCPU entry to the final halt, including
host function calls. Its target is `hyperlight_spans` and its level is
`TRACE`, so filters such as `info` leave it off. Guest output is emitted inside
it. In OpenTelemetry, root guest spans go under it.

Where each span runs:

| Span | Runs in | Created by | Times from |
|---|---|---|---|
| Spans in guest code, such as `#[instrument]` | Guest VM | Guest code | Guest TSC |
| `call_guest_function` | Guest VM | `hyperlight-guest-bin`, at `TRACE` | Guest TSC |
| `call-to-guest` | Host, on the calling thread | `hyperlight-host`, at `TRACE` | Host clock |
| Other `hyperlight-host` spans | Host | `hyperlight-host`, at `TRACE` | Host clock |
| Spans in host functions | Host | Embedder | Host clock |

A guest span's duration includes time the guest spent stopped while the host
handled its requests, such as host function calls.

The host emits guest output on the thread that called the guest:

* Guest events arrive while the call runs. Each one exits to the host, which
  emits it right away.
* Guest spans are not host `tracing` spans, because `tracing` cannot create a
  span with past start and end times. The guest records its spans and sends
  them in batches. At the end of the call, the host emits one span close event
  per guest span, parents before children. The guest's times are in the
  event's fields.
* Span close events have no parent span.

Targets and [fields](#fields) are stable API.

#### Example

Guest code in the module `simpleguest`:

```rust
#[instrument]
fn handle(user: u32) {
    info!("handling request");
}
```

The host uses `tracing_subscriber::fmt` and calls `handle(42)` in a sandbox
with `guest_log_level(LevelFilter::DEBUG)`.

With `EnvFilter` `info,hyperlight_spans=off`, one line appears:

```text
INFO hyperlight_guest: handling request call_id=7 guest_file="src/main.rs" guest_line=12 guest_module="simpleguest" fields="{}"
```

With `EnvFilter` `info`, two lines appear:

```text
INFO hyperlight_guest: handling request call_id=7 guest_file="src/main.rs" guest_line=12 guest_module="simpleguest" guest_span_id=1 fields="{}"
INFO hyperlight_spans: call_id=7 guest_span_id=1 name="handle" guest_target="simpleguest" start_unix_ns=1795029435153000000 end_unix_ns=1795029435153200000 fields="{\"user\":\"42\"}"
```

With `EnvFilter` `info,hyperlight_spans=trace`, the guest event line gains the
prefix `call-to-guest{call_id=7 function="handle"}:`. The span close event has
no parent span, so its line has no prefix.

Timestamps and other host spans are left out of these lines.

### OpenTelemetry

`hyperlight-otel` is a new crate. Its layer turns span close events into
OpenTelemetry spans with the guest's times. Parents arrive first, so each span
is built under its parent's real OpenTelemetry context. Root guest spans go
under the current OpenTelemetry context. That is `call-to-guest` when the
filter enables it, otherwise the embedder's current span, otherwise a new
trace. Guest spans have the attributes `hyperlight.guest = true`,
`hyperlight.call_id` and `hyperlight.guest_span_id`.

`tracing-opentelemetry` records guest events as span events on the current
span, as it does for any event. It skips span close events, because the host
emits them with `parent: None`.

The layer takes the embedder's tracer, so it never uses
`opentelemetry::global`. Each `hyperlight-otel` release names the
`opentelemetry` version it supports. A mismatch is a compile error.
`hyperlight-host` no longer depends on `opentelemetry` or
`tracing-opentelemetry`.

```rust
tracing_subscriber::registry()
    .with(tracing_opentelemetry::layer().with_tracer(tracer.clone()))
    .with(hyperlight_otel::layer(tracer))
    .with(EnvFilter::new("info"))
    .init();

let sandbox = SandboxBuilder::from_file(path)
    .guest_log_level(LevelFilter::INFO)
    .build()?;
```

The two layers can be added in either order.

### Guest code

Guest code should use the `tracing` macros, such as `info!`, `span!` and
`#[instrument]`. Only `tracing` can produce guest spans.

Libraries that only use `log`, such as `wasmtime`, need no workaround. Their
`log` macros make guest events, with their level. They make no guest spans.
The C API `hl_log` works the same way.

### Overhead

The baseline is today's default build, without the host `trace_guest`
feature. A guest call in the `guest_calls/call/default` benchmark takes about
15 µs. The costs below are estimates built from measured components.

| Setup | Estimated added cost per guest call |
|---|---|
| No sandbox level, the default | None |
| Sandbox level set, spans off | Up to 24 ns, under 0.2% |
| Sandbox level set, spans on | Up to 24 ns, plus decoding |
| `call-to-guest` enabled, such as with `hyperlight_spans=trace` | 0.4 to 0.6 µs more, 3 to 4% |

With a sandbox level set, the guest skips output the filter would drop. This
can save VM exits compared with today.

With spans on, the cost is decoding the guest's spans. Decoding grows with
the number of guest spans and was not measured.

Today's host `trace_guest` feature costs 1 to 1.5 µs, 7 to 10%, on every
guest call, even when nothing records spans. Most of it is two `CPUID`s per
call, and each one exits to the hypervisor. The feature also reads the
OpenTelemetry context on every vCPU entry and the guest's registers on every
OUT exit. The proposal removes all of this.

The guest pays one atomic load per disabled `tracing` macro, and reads its
level slot once per call as it does today. Every guest carries the span
recorder, about 41 KB.

[Measurements](#measurements) has the details.

### Migration

These are breaking changes. Each gets a `CHANGELOG.md` entry.

* Guest output is off unless the embedder sets a sandbox level.
* `RUST_LOG` no longer sets the guest level.
* Guest output needs a `tracing` subscriber. Hyperlight does not forward to
  `log` loggers. An embedder that uses only a `log` logger, such as
  `env_logger`, installs a `tracing` subscriber. `tracing_subscriber::fmt`
  with `EnvFilter` accepts the same `RUST_LOG` syntax. A layer can forward
  events to an existing logger. Hyperlight's own host events also need a
  `tracing` subscriber.
* OpenTelemetry users add `hyperlight-otel`.
* Builds that enable the host or guest `trace_guest` feature, including the
  Justfile and fuzz targets, drop it.
* Callers of `guest_logger::log_message` use the `log` or `tracing` macros.
* Guests cannot install their own global `tracing` subscriber.
  `hyperlight-guest-bin` installs one.
* Hyperlight no longer logs errors it returns. Embedders log the errors they
  care about.

## Design

### Host

* With a sandbox level, before each guest call the host computes two levels:
  * Event level: the less verbose of the sandbox level and the most verbose
    level the subscriber might enable for `hyperlight_guest`.
  * Span level: the same, for `hyperlight_spans`.
* The host reads only cached callsite interest. Interest `sometimes` counts as
  enabled, because the answer can depend on the span an event is emitted in.
  The subscriber filters exactly at emission.
* It sends the levels to the guest when they change, and always at init,
  `from_snapshot` and `restore`. An unset sandbox level is sent as `OFF`.
* The levels are a hint, so the guest can skip guest output without a VM
  exit. The host still checks each guest event and span against the sandbox
  level before emitting it. A guest that ignores its level cannot get past the
  sandbox level.
* `call_id` and `call-to-guest` exist while a sandbox level is set. Trace
  state exists while the span level is not `OFF`.
* One handler reads trace messages, in both places the host drains the queue.
  It always acknowledges them. It drops the payload when the span level is
  `OFF`, a limit is reached, or the payload is malformed. Only a broken queue
  entry is a transport error. Trace data never changes the result of a guest
  call.
* The host limits trace state per call: open spans, span records and bytes.
  The defaults are documented.
* At the end of a call, spans still open are reported as `incomplete`. The
  host knows only spans whose open record arrived. Their later close records
  are dropped.
* Guest timestamps are clamped to the call's time window, with the end no
  earlier than the start. Control characters in guest text are escaped.
* The TSC frequency is measured once per process.

### Guest

* `hyperlight-guest-bin` always installs a small `tracing` subscriber. It
  sends guest events to the host on the guest-to-host queue, as today.
* On x86_64, the subscriber also records spans while the span level is not
  `OFF`. While it is `OFF`, span callsites have interest `never`, so a span
  macro costs one load. The span buffer is allocated when spans first turn
  on.
* `hyperlight-guest-bin` keeps its `log::Log` implementation as the `log`
  bridge.
* Trace batches use the guest-to-host queue as a new message kind. They hold
  span open and close records. Open records carry the span's level. The guest
  sends a batch when its buffer fills, at the end of each guest call, and on
  abort when it can.
* Guest event messages gain the current guest span ID and the event's fields.
* `guest_logger::log_message` becomes private.

The guest receives its two levels through the existing scratch slot. This
changes the snapshot ABI, so `SNAPSHOT_ABI_VERSION` increases. The host
rejects guests whose version note names another release. It accepts guests
with no version note, as for earlier ABI changes.

### Cargo features

* `hyperlight-host` drops the `tracing` crate's `log` Cargo feature.
* The host and guest `trace_guest` features are removed. Each stays for one
  release as an empty, deprecated feature.
* The span recorder is always compiled on x86_64. Today it adds 41 KB to a
  release guest. Its size is measured again once rewritten.
* Every guest runs on every host (#1863).
* `mem_profile` no longer depends on `trace_guest`. Its own port problem from
  #1863 is out of scope.

### Hyperlight's own output

Under a filter such as `info`, an embedder sees its own output and its guests'
output. Hyperlight adds nothing unless something goes wrong that the embedder
cannot otherwise see.

* Hyperlight's own spans, in the host and guest crates, are at `TRACE`.
  `call-to-guest` replaces the span on `Sandbox::call`. The guest spans
  `internal_dispatch_function` and `generic_init` are removed, because
  `call-to-guest` covers them.
* Diagnostic events, such as build information or hypervisor detection, are
  at `DEBUG`.
* An error returned to the caller is not logged above `DEBUG`. The error
  carries its own context. `#[instrument(err)]` uses `err(level = "debug")`,
  and `log_then_return!` logs at `DEBUG`.
* An error Hyperlight drops is logged at `WARN`. Examples are failed LAPIC
  writes in the MSHV interrupt path and a failed timer eventfd write in KVM.
  Best-effort cleanup that runs before returning another error is not logged.
* A failure in `Drop` is logged at `ERROR`, because it cannot be returned.

## Reference

### Sandbox level methods

`SandboxBuilder::guest_log_level`, `Sandbox::log_level`,
`SandboxConfiguration::set_max_guest_log_level` and
`UninitializedSandbox::set_max_guest_log_level`. They keep their names and
their `LevelFilter` type.

### Fields

* Guest event, target `hyperlight_guest`, with the guest's message:
  * `call_id`: the guest call the event came from.
  * `guest_file`, `guest_line` and `guest_module`.
  * `guest_span_id`: the guest span the event was emitted in. Absent outside a
    span, or when spans are off.
  * `fields`: the event's own fields, as a JSON object with string values. For
    example, `info!(user = 42, "handling")` gives `{"user":"42"}`.
* Span close event, target `hyperlight_spans`:
  * `call_id`, `guest_span_id` and `guest_parent_id`. `guest_parent_id` is
    absent for a root span.
  * `name` and `guest_target`.
  * `start_unix_ns` and `end_unix_ns`.
  * `fields`: the span's own fields, as JSON like guest events.
  * `incomplete`: `true` when the span was still open at the end of the
    call. `end_unix_ns` is then the end of the call.
* Host span `call-to-guest`, target `hyperlight_spans`, level `TRACE`:
  `call_id`, and `function`, the guest function called. `function` is absent
  for guest init.

`call_id` is unique in the process. Guest span IDs are unique within one
`call_id`. Consumers key guest spans by `(call_id, guest_span_id)`, so the
key works even when a filter drops `call-to-guest`.

The host emits a call's span close events at the end of the call, parents
before children. A parent can be filtered out by level. Consumers treat a
missing parent as the call's root.

### Measurements

* The host probes the subscriber with its own static callsites and reads
  their cached interest. The measured probes also asked the subscriber when
  the interest was `sometimes`. They did more work than the proposed probes,
  so the proposed cost is at most the measured one. `tracing::enabled!` asks
  the subscriber every time, even when the cached interest is `always`.
* A microbenchmark, with `tracing` 0.1.44, `tracing-subscriber` 0.3.23 and a
  `fmt` layer writing to `io::sink`, on x86_64:

  | Work | Cost |
  |---|---|
  | Level probes, no `tracing` subscriber | 3 ns |
  | Level probes, filter `info` or `warn` | 5 to 8 ns |
  | Level probes, filter `info,[request]=debug` | 20 to 24 ns |
  | The same probes with `tracing::enabled!` | 37 to 92 ns |
  | One `call-to-guest` span, when the filter enables it | 400 to 600 ns |
  | Two `CPUID`s, host in a Hyper-V VM | 700 ns |

* The `trace_guest` cost is the difference in `guest_calls/call/default`
  between builds with and without the feature. It was measured over three
  alternating runs pinned to one CPU, with no `tracing` subscriber. The
  per-exit register read stayed below the noise, about 3 µs, of
  `guest_calls/call_with_host_function`.
* Guest size, from `size -A`, for release guests built with and without the
  guest `trace_guest` feature:

  | Guest | Without | With | Added |
  |---|---|---|---|
  | `simpleguest` | 495,164 | 536,292 | 41,128 bytes, 8.3% |
  | `witguest` | 387,532 | 428,660 | 41,128 bytes, 10.6% |

* Guest call time with the span recorder compiled in and the guest level
  `OFF`, against a guest without it. Both ran in one process, in alternating
  blocks of 50 calls, over 2,000 rounds pinned to one CPU. The median paired
  difference stayed within ±0.4 µs of zero over three runs, for `Echo` at 18
  to 29 µs and for a call with one host function call at 34 to 49 µs.

## Test plan

Performance:

* Benchmarks against today's host built without `trace_guest`: no sandbox
  level, sandbox level set with spans off, events only, spans on, and calls
  with many host function calls. With no sandbox level there must be no
  measurable change. With a sandbox level and spans off, report the cost per
  call for a target-only filter and a span-based filter.
* Guest size and call time with the span recorder and spans off, against a
  guest without the recorder.

Levels and snapshots:

* A snapshot taken at `DEBUG` and restored into a sandbox with no level sends
  nothing.
* A guest that ignores its level sends nothing past the sandbox level.
* Filter reload applies from the next call.
* A DEBUG guest event that the filter enables only inside `call-to-guest`
  reaches the subscriber.
* Two sandboxes from one snapshot at `warn` give distinct `call_id`s.
* A snapshot file round trip keeps guest spans working.

Output:

* Tests run guest spans end to end with the default test guests, with spans
  on and off.
* `info,hyperlight_spans=off` gives guest events and no span close events.
* Guest events carry `call_id` with `hyperlight_spans=off`.
* A field-only event, such as `info!(user = 42)`, keeps its field.
* Logging and tracing tests run in parallel with the rest of the suite. Each
  test binary installs one global subscriber. Its callsite interest is always
  "sometimes", and it captures events per thread. Per-test `with_default`
  loses events (tokio-rs/tracing#2874).

OpenTelemetry:

* `hyperlight-otel` with a private in-memory tracer, no global provider,
  exports each guest span once. Each exported parent span ID matches a span
  the exporter received.
* With `tracing-opentelemetry` and `hyperlight-otel` in either order, each
  guest event and guest span reaches OpenTelemetry once.
* With filter `warn`, WARN guest spans export under the embedder's span, or as
  a new trace.

Robustness:

* A guest abort inside a span gives an `incomplete` span when the open record
  was sent, and nothing otherwise. The same holds for a cancelled call.
* A rejected trace message before a valid result returns that result.
* Guest timestamps outside the call are clamped.
* A normal guest call under filter `info` gives no Hyperlight output.
* A guest that sends malformed data makes the host log nothing above `DEBUG`.
* The trace decoder is fuzzed.

## Drawbacks

* Guest `error` events are off by default. Guest panics and aborts still reach
  the embedder as call errors.
* Libraries that wrap Hyperlight, such as hyperlight-wasm, must pass the guest
  level through.
* With a sandbox level set, guest spans are on under a plain filter such as
  `info`. `fmt` prints one line per guest span. `hyperlight_spans=off` turns
  them off, but only for the whole process. A per-sandbox span setting can be
  added later without a breaking change.
* Event targets and field names become API.
* Guest fields become JSON text with string values.
* All guest events share one target, so filters cannot select one guest
  module.
* OpenTelemetry users add a crate.
* In OpenTelemetry, guest events sit on the current span, such as
  `call-to-guest`, not inside their guest span. They carry `guest_span_id`.
* In `fmt`, span close events have no span prefix, because they are root
  events. `call_id` links them to their call.
* Guest spans appear at the end of their call, not while it runs.
* Host function calls from the guest have no span of their own. Embedder spans
  inside a host function go under `call-to-guest`, or the embedder's span.
* Under a filter such as `info`, `call-to-guest` is off. In OpenTelemetry,
  root guest spans then go under the embedder's span, or each starts its own
  trace.
* The levels sent to the guest cannot express filters on spans or field
  values. The guest then sends guest output that the subscriber drops.

## Alternatives

* A trace sink on the sandbox builder. It is typed, but configured apart from
  the subscriber.
* A layer the host finds by type, as `tracing-error` does. A missing layer, or
  one from another Hyperlight version, turns spans off with no error.
* An OpenTelemetry tracer passed to the sandbox builder. It puts
  `opentelemetry` 0.x types in the `hyperlight-host` API.
* Separate per-sandbox levels for guest events and guest spans. More control,
  but two settings where `tracing` uses one.
* A `call-to-host` span under the guest span that made the call. Every host
  function call request would carry the active guest span ID, and
  OpenTelemetry cannot reparent a started span. It can be added later.
* OpenTelemetry span IDs derived from `call_id` and `guest_span_id`, so spans
  can be exported as they close. The `opentelemetry` SDK always generates its
  own span IDs.
