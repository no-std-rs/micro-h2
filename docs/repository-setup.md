# Repository setup

The local layout is a bare repository at `micro-h2/.git` and its `main`
worktree at `micro-h2/main`. The public remote is
`https://github.com/no-std-rs/micro-h2.git`.

## Create the public repository and push

With authenticated GitHub CLI access and organization repository-creation
permission, run from the checkout:

```sh
bash tools/setup-github.sh
```

The script creates the public repository if absent, configures squash merges
with Conventional Commit PR titles, pushes the local commits, adds repository
topics, and protects `main` with required CI and PR-title checks. It refuses an
existing private repository and never force-pushes. It allows administrator
bypass and requires no second approver, so a solo maintainer can operate it.
GitHub Actions must be allowed by the organization policy.

## Automated releases

The release workflow uses the same GitHub App and crates.io OIDC pattern as
`oci-zero`. Both jobs stay disabled until `RELEASE_PLZ_ENABLED` is `true`, so
the initial push does not fail for missing release credentials.

1. Install the release-plz GitHub App on `no-std-rs/micro-h2`. Give it contents
   and pull-request write access. Using the App lets release PRs trigger CI.
2. Make `RELEASE_PLZ_APP_ID` and `RELEASE_PLZ_APP_PRIVATE_KEY` available as
   repository secrets, or extend existing organization-secret access to this
   repository. Keep private keys out of Git and command-line arguments.
3. In the existing `micro-h2` crate's crates.io trusted-publishing settings,
   configure owner `no-std-rs`, repository `micro-h2`, and workflow
   `release-plz.yml`, with no environment. This repository uses the existing
   crate name and starts with its `0.0.1` version; do not republish that version.
4. Enable and run the workflow:

   ```sh
   gh variable set RELEASE_PLZ_ENABLED --repo no-std-rs/micro-h2 --body true
   gh workflow run release-plz.yml --repo no-std-rs/micro-h2
   ```

Release-plz compares against the published crate, opens a version/changelog PR
when eligible changes require a release, and publishes the bumped version
after that PR merges. CI validates packaging without publishing.

## CI and updates

CI runs on pull requests and pushes to `main`. Stable Rust checks formatting,
Clippy, all unit/differential/doc tests, docs, and packaging. Rust 1.88 checks
the library without `std` on bare-metal RISC-V and WebAssembly. Cargo-deny
checks advisories across runtime and development dependencies. Weekly
Dependabot PRs update SHA-pinned actions and Cargo dependencies; Cargo.lock is
committed, and CI uses `--locked`.
