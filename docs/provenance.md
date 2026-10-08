# Extraction provenance

The initial import comes from `pawelchcki/tailfeather` at commit
`f38b22bdb49ace2faa81827e2e569889505bb6f5`:

- `crates/micro-h2/`: implementation, package metadata, README, and licenses.
- `crates/ts-conformance/tests/h2_differential.rs`: HTTP/2 interoperability tests.
- `crates/ts-conformance/tests/hpack_differential.rs`: HPACK interoperability tests.

The original source and tests are preserved in the first commit. Subsequent
commits adapt package metadata, documentation, and CI for standalone use.
The original dual MIT/Apache-2.0 license and copyright attribution are retained.
Tailfeather's workspace-specific Bazel configuration is not part of this
Cargo-based repository.
