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
4 KiB header-block buffer, no server mode, no push, no priority tree, no
trailers, and no outgoing CONTINUATION frames. Unsupported features are refused
instead of partially implemented.

TLS, TCP, retries, and request scheduling belong to the caller. This crate only
turns HTTP/2 bytes into bounded state and events.

## Verification

Unit tests cover framing, HPACK state, Huffman decoding, and flow control.
Differential tests in `tests/` exchange traffic with the Rust `h2`
implementation, including a 4 MiB response that exercises flow-control updates,
and compare HPACK in both directions with `fluke-hpack`. The tests use in-memory
transports and require no external service.

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
and [repository setup](docs/repository-setup.md) for release-plz and crates.io
trusted publishing. The library and its interoperability tests were extracted
from Tailfeather; [provenance](docs/provenance.md) records the source snapshot.

## License

MIT OR Apache-2.0, at your option.
