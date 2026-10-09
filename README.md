# micro-h2

[![CI](https://github.com/no-std-rs/micro-h2/actions/workflows/ci.yml/badge.svg)](https://github.com/no-std-rs/micro-h2/actions/workflows/ci.yml)
[![crates.io](https://img.shields.io/crates/v/micro-h2.svg)](https://crates.io/crates/micro-h2)
[![docs.rs](https://docs.rs/micro-h2/badge.svg)](https://docs.rs/micro-h2)

A small, client-only HTTP/2 implementation for systems without `std` or an
allocator.

It is sans-I/O: the caller moves bytes to and from a socket while `Connection`
owns the HTTP/2 and HPACK state. It requires Rust 1.88 and forbids unsafe Rust.
The runtime dependency is `heapless`; HTTP/2 and HPACK reference implementations
are used only in tests.

```toml
[dependencies]
micro-h2 = "0.0.1"
```

```rust
let mut connection = micro_h2::Connection::new();
let mut out = [0u8; 256];
let written = connection.start(&mut out).unwrap();
// Send out[..written] through the transport you provide.
```

This is experimental protocol software; the supported HTTP/2 surface is
deliberately limited as described below.

## What it handles

- the client preface and SETTINGS exchange
- bounded request streams with HEADERS and optional DATA
- response HEADERS, CONTINUATION, and DATA frames
- connection and stream flow control
- PING, RST_STREAM, GOAWAY, padding, and unknown frame types
- HPACK static and dynamic tables, including Huffman decoding

The usual flow is:

1. Create a `Connection` and send the bytes from `Connection::start`.
2. Open a stream with `Connection::request`.
3. Feed each complete frame to `Connection::recv`.
4. Send any acknowledgement or WINDOW_UPDATE bytes it writes, and act on the
   returned `Event`.

## Deliberate limits

This is not a general browser HTTP/2 stack. It has four concurrent streams, a
2 KiB header-block buffer, no server mode, no push, no priority tree, no
trailers, and no outgoing CONTINUATION frames. Unsupported features are refused
instead of partially implemented.

Request bodies must fit one DATA frame and both available send windows.
`Error::FlowControl` leaves the connection unchanged; process peer window updates
or SETTINGS before retrying. New requests also respect MAX_CONCURRENT_STREAMS
and stop after GOAWAY. Discard the connection after a receive-side protocol or
HPACK error.

Responses must belong to a client-opened stream and carry exactly one valid
`:status` in the range 100–599 before ordinary fields. Informational responses
may precede the final headers; DATA requires final headers. Header names must be
lowercase tokens, values must obey HTTP/2 field syntax, and connection-specific
fields are refused.
Content-Length counts unpadded body bytes and must match at END_STREAM, with the
HEAD, 204, and 304 response exceptions. HEAD, 204, 205, and 304 responses cannot
deliver content. Malformed blocks are rejected before any header callback runs.
Late frames on closed streams produce no application events, while preserving
connection flow control and HPACK state. DATA after a successful CONNECT carries
tunnel bytes and ignores response Content-Length.
Outgoing fields and declared body lengths are checked before opening a stream
or writing output; consistent duplicate Content-Length values are emitted once.

Transport adapters that consume DATA incrementally can call `finish_data` for
unpadded frames. Padded frames require `finish_data_with_length`, supplying the
content length after the adapter validates and consumes the padding. Both paths
enforce response state and body lengths, and permit a larger-output-buffer retry
without counting the same bytes twice.

TLS, TCP, retries, and request scheduling belong to the caller. This crate only
turns HTTP/2 bytes into bounded state and events.

## Verification

Unit tests cover framing, HPACK state, Huffman decoding, and flow control.
Differential tests in `tests/` exchange traffic with the Rust `h2`
implementation, including a 4 MiB response that exercises flow-control updates,
and compare HPACK in both directions with `fluke-hpack`. Protocol regressions
fill all four stream slots on eight independent connections: a barrier ensures
32 simultaneous streams, with 64 round trips across two waves. Each response
exceeds the receive window; tests verify exact headers and echoed bytes,
credit-buffer retries, negotiated limits, and valid and malformed continuation
sequences. The tests use in-memory transports and require no external service.

```sh
cargo fmt --all --check
cargo clippy --locked --all-targets --all-features -- -D warnings
cargo test --locked --all-features
cargo check --locked --lib --no-default-features --target riscv32imac-unknown-none-elf
cargo check --locked --lib --no-default-features --target wasm32-unknown-unknown
```

Install the cross-compilation targets with `rustup target add` before checking
them locally. CI checks both with the Rust 1.88 minimum supported version.

## Contributing and releases

See [AGENTS.md](AGENTS.md) for validation and Conventional Commit PR titles,
and [repository setup](docs/repository-setup.md) for Runnerless release-please and crates.io
publishing. The library and its interoperability tests were extracted
from Tailfeather; [provenance](docs/provenance.md) records the source snapshot.

## License

MIT OR Apache-2.0, at your option.
