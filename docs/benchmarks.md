# Throughput and bounded-memory benchmarks

These are two independent modes, both using the default `Connection` and the
public API. They add no runtime dependencies or library features. They measure
the sans-I/O engine and caller-owned delivery buffers; sockets, TLS, and an
executor belong to the embedding application.
The benchmark profile uses thin LTO and one code-generation unit; the default
library release profile is unaffected.

## Maximum throughput

```sh
cargo bench --locked --bench throughput
# Optional host-specific code generation:
RUSTFLAGS='-C target-cpu=native' cargo bench --locked --bench throughput
```

Each case compares four paths:

| Path | Timed work |
| --- | --- |
| memcpy | Copy the DATA payload using `copy_from_slice`. |
| recv + copy | Parse the complete frame, validate response state, account for body length and flow control, borrow `Event::Data`, then copy it into the application buffer. |
| copy + finish_data | Parse the frame prefix, copy the payload directly into the application buffer, then commit response state and flow-control credit with `finish_data`. This models an adapter that delivers authenticated body bytes directly. |
| recv borrowed | Parse and validate the frame and expose a borrowed DATA slice, including flow control. This does not read or copy the body, so it reports ns/frame rather than a memory-bandwidth claim. |

The three copy paths use the same source frames, destination, number of bytes,
per-frame compiler barriers, and frame size. Payload sizes are 1, 4, and 16 KiB;
16 KiB is the receive-side maximum advertised by the library. Two working sets
are reported: one reused frame and sink, and a sequential sweep over 64 MiB of
payload plus its frame prefixes, with a separate 64 MiB destination. The larger
case exposes cache and memory effects; whether it exceeds the last-level cache
depends on the host. The memcpy baseline uses the same frame-prefixed source
layout, skipping the nine-byte prefixes.

Allocation, frame construction, connection initialization, SETTINGS exchange,
request encoding, response-header decoding, and initial page faults happen
before timing. Each sample lasts at least 100 ms, at complete batch boundaries.
Seven samples are taken per path, rotating their order, and the median elapsed
time per payload byte is reported. Results use decimal GB/s, ns/frame, and the
ratio **path throughput / memcpy throughput**. WINDOW_UPDATE generation is
timed; sending those updates through a transport is outside this benchmark.

`black_box` keeps the copies, borrowed slices, and generated control frames
observable. Outside timing, all three copy paths are checked byte for byte,
the borrowed pointer is checked against the input payload, and END_STREAM is
checked to release the stream slot. Timing uses a sustained response without
Content-Length; connection-level and stream-level flow control remain active.

Near-memcpy performance is a measured result for a particular host and frame
size, rather than a threshold the benchmark assumes. Small frames can expose
protocol overhead; larger streaming copies can be limited by memory bandwidth.
No absolute or relative timing gate is imposed in CI. For comparisons, record
the commit, Rust version, CPU, compiler flags, and output; use an otherwise idle
machine and rerun to assess variation. Pinning to one CPU with `taskset` can
reduce scheduling noise on Linux.

## 50 kB workload heap budget

```sh
cargo bench --locked --bench memory
# Short budget/correctness check, also run in CI:
cargo bench --locked --bench memory -- --quick
```

The budget is **50,000 bytes**, which is stricter than 50 KiB (51,200 bytes).
A single caller-owned `Box<Workspace>` includes:

- the `Connection`, including HPACK storage, stream state, and header reassembly;
- a bounded scripted peer's credit counters and verification state;
- a complete-frame buffer and application sink, or small incremental buffers;
- the output buffer used for requests, SETTINGS acknowledgements, and updates.

A benchmark-only global allocator forwards to `System` and counts live
requested allocation bytes and successful allocation/reallocation calls. Before
workspace construction it establishes a baseline for the benchmark host's
existing allocations and imposes a 50,000-byte additional live-allocation limit.
Any allocation exceeding that limit returns null, causing ordinary Rust
allocation to fail. The budget covers setup, handshake, request encoding, header
decoding and callbacks, response delivery, verification, and workspace teardown.
No printing or timing-result collection occurs inside the allocation scope.
Peak heap use must equal `size_of::<Workspace>()`, exactly one setup allocation
must occur, and teardown must return to the original baseline. This catches
even transient protocol allocations. Before each benchmark run, an allocator
self-check verifies limit rejection, zeroed allocation, realloc growth/shrink,
and return to baseline. Unsafe code is confined to this instrument in the std
benchmark; the library continues to forbid unsafe Rust.

Each adapter runs eight waves of four simultaneously open POST requests with
512-byte bodies. Every response has 4 MiB of content, totaling 128 MiB delivered
and checked per adapter. DATA frames are interleaved across streams. The peer
spends connection and stream credit before delivery and consumes every emitted
WINDOW_UPDATE; stalled, inflated, or incomplete credit fails the benchmark.
Every received byte is copied into the bounded sink and checked against a
stream- and offset-dependent pattern. Content-Length and END_STREAM enforce
the response length; all stream slots must be released before the next wave.
Response headers are split across HEADERS and CONTINUATION, with a dynamic-table
insertion followed by indexed reuse across streams and waves.

The complete-frame adapter receives the maximum 16 KiB DATA payload and copies
its borrowed bytes into a reusable 16 KiB sink. The incremental adapter receives
the same size wire frames in 512-byte chunks and calls `finish_data` only after
consuming and checking the whole payload. Its smaller workspace demonstrates
that frame size and advertised flow-control windows do not require retaining a
whole body or even a complete DATA frame.

The reported MB/s includes peer payload generation, every-byte verification,
setup, and teardown, so it is not comparable to the throughput mode's copy-only
baseline. The memory limit excludes stack, executable/static storage, allocator
metadata/internal fragmentation, and the benchmark host's existing allocations.
It is evidence for bounded workload heap use, not a claim that the entire
process or a TCP/TLS application uses 50 kB of total RAM. The existing `h2`
differential tests separately verify interoperability with an independent peer;
this bounded peer avoids including that peer's heap in the client budget.

Both modes accept `--quick` (and Cargo's `--test`) for shorter runs. Throughput
then uses three samples of at least 10 ms; memory uses two waves and still checks
32 MiB per adapter. Memory-size and correctness assertions remain enabled in
optimized benchmark builds.

## Example measured results

Measured on 2026-10-09 with these benchmark changes based on `422ff4e`, on an
AMD Ryzen 9 7900, Linux x86_64, Rust 1.97.1, using the commands above with the
repository's bench profile and no extra compiler flags or CPU pinning. These
are one host's results, not portable performance guarantees.

| Working set | Payload/frame | memcpy GB/s | recv + copy GB/s | copy + finish_data GB/s | Borrowed ns/frame |
| --- | ---: | ---: | ---: | ---: | ---: |
| Hot | 1 KiB | 118.72 | 64.88 | 61.79 | 8.1 |
| Hot | 4 KiB | 150.02 | 113.37 | 115.23 | 8.1 |
| Hot | 16 KiB | 142.95 | 124.18 | 123.32 | 8.2 |
| 64 MiB sweep | 1 KiB | 15.26 | 13.82 | 13.66 | 8.3 |
| 64 MiB sweep | 4 KiB | 14.30 | 13.87 | 13.51 | 8.2 |
| 64 MiB sweep | 16 KiB | 15.59 | 15.42 | 15.48 | 7.8 |

At 16 KiB per frame in the sweep, both protocol-plus-copy paths reached 99% of
the memcpy baseline. Hot buffers and smaller frames expose more protocol cost;
their results are included to show that tradeoff.

| Memory adapter | Peak workload heap | Headroom under 50,000 B | Setup allocations | Protocol allocations | Body bytes verified |
| --- | ---: | ---: | ---: | ---: | ---: |
| Complete 16 KiB frames | 41,040 B | 8,960 B | 1 | 0 | 134,217,728 |
| Incremental 512 B chunks | 9,280 B | 40,720 B | 1 | 0 | 134,217,728 |

`Connection` occupied 6,064 bytes on this target. Each adapter checked 512
WINDOW_UPDATE frames across eight waves. Sizes can vary with target alignment
and pointer width; the executable measures and asserts its actual target's
workspace size.
