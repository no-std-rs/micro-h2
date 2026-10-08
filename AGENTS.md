# Contributor guide

`micro-h2` is a `no_std`, allocation-free HTTP/2 client. Keep sockets, TLS,
executors, and allocation in callers. The library forbids unsafe Rust and
supports Rust 1.88. Preserve bounded storage and the existing public API when
making compatible changes.

## Validation

Run `cargo fmt --all --check`, `cargo clippy --locked --all-targets --all-features
-- -D warnings`, and `cargo test --locked --all-features`. CI also checks docs,
packaging, advisories, and Rust 1.88 builds for bare-metal RISC-V and WebAssembly.
The tests in `tests/` compare HTTP/2 with `h2` and HPACK with `fluke-hpack`.
Keep Cargo.lock committed so CI and dependency updates are reproducible.

## Pull requests and releases

PR titles must use Conventional Commits because squash merges use the PR title
as the commit message. Prefer scope `micro-h2` and a lowercase imperative
subject, for example `fix(micro-h2): reject an invalid stream identifier`.

Use `feat`, `fix`, or `perf` for changes that should trigger a release;
`refactor`, `docs`, `test`, `ci`, `build`, and `chore` do not trigger one.
Add `!` or a `BREAKING CHANGE:` footer for breaking changes. This policy is
enforced by `.release-plz.toml` and the PR-title workflow.

Once configured, release-plz opens version/changelog PRs, then publishes after
they merge using crates.io trusted publishing. Do not run `cargo publish` by
hand. See `docs/repository-setup.md` for required GitHub App and crates.io setup.
