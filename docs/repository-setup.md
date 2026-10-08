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

## Codex review through Runnerless

`.runnerless-ci.ts` evaluates trusted Codex evidence and reports
`Runnerless / Codex review`. The check passes only when the shared evaluator
accepts the current head, including resolved review threads and no newer
unanswered review request. The evaluator retains its maintainer-authorized
`codex: bypass-review` escape hatch.

Activation requires these settings:

1. Give My Toolkit (GitHub App `4602759`) and the Codex connector access to
   this repository. Enable automatic code review for `no-std-rs/micro-h2` in
   [Codex review settings](https://chatgpt.com/codex/settings/code-review), or
   request `@codex review` on each new head. The `codex: auto-review` PR label
   also lets the shared tracker request a review when evidence is missing.
2. Deploy the scoped bridge configuration from `pawelchcki/my-infra`: add
   this repository to `ALLOWED_REPOSITORIES` for review evidence tracking and
   to `RUNNERLESS_FORWARD_REPOSITORIES` for program execution. Keep the
   account-wide allowlist and legacy enforcement canaries unchanged.
3. After the program is on `main`, open a PR and confirm Runnerless reports
   its review check. Pin that check to its App while preserving existing CI
   requirements:

   ```sh
   bash tools/enable-codex-gate.sh PR_NUMBER
   ```

The activation script refuses to require an unseen check. The gate is active
only after this branch-protection update; committing the program alone does
not enforce merges. Administrator bypass remains as configured by the initial
repository setup. `tools/setup-github.sh` retains an already-required
Runnerless review check when rerun.

## Automated releases

Runnerless owns the release lifecycle. `.runnerless-ci.ts` declares
`workflow().releasePlease()`; `.ci-toolkit.yml` configures Conventional Commit
versioning and changelog generation. `version.txt` starts at the already
published `0.0.1`; never republish that version. A release PR bumps that file and
CHANGELOG.md. Merging the PR makes Runnerless create its tag and GitHub Release.

The secret-free `publish-release.yml` workflow checks out that tag, aligns
Cargo.toml and Cargo.lock with version.txt, tests and verifies the crate, captures
Cargo's registry upload against a local loopback endpoint, and attaches the
`.cargo-upload` body and SHA256SUMS to the GitHub Release. The successful
workflow_run event asks Runnerless to verify and publish those assets to crates.io.
GitHub Actions has no crates.io credential.

Before activation:

1. Install My Toolkit App `4602759` on this repository. The Runnerless bridge
   must list `no-std-rs/micro-h2` in RUNNERLESS_FORWARD_REPOSITORIES and
   RELEASE_PROGRAM_REPOSITORIES, with a registry grant for only `micro-h2`.
2. Connect the repository in Runnerless, add a crates.io publishing target for
   `micro-h2` with tag prefix `v`, store its publishing token through the
   write-only Package publishing form, and enable Production. A host fallback
   grant uses binding REGISTRY_MICRO_H2_CRATES_TOKEN; credentials never go in Git.
3. Deploy the reviewed bridge settings before merging this release migration.
   Keep release-plz disabled; its workflow is removed by the migration so only
   Runnerless can create releases.

A push to main reconciles release state. After a release PR merges, monitor
`Runnerless / Release`, the packaging workflow, and
`Runnerless / crates-micro-h2`; then verify the new version on crates.io.
To rebuild a published GitHub release whose packaging failed, dispatch
publish-release.yml with its existing tag. Existing assets are not overwritten;
inspect any attached partial output before a retry.

The Codex review gate can be activated separately using
`tools/enable-codex-gate.sh` after its current-head check is observed. It is not
required to configure release-please.

## CI and updates

CI runs on pull requests and pushes to `main`. Stable Rust checks formatting,
Clippy, all unit/differential/doc tests, docs, and packaging. Rust 1.88 checks
the library without `std` on bare-metal RISC-V and WebAssembly. Cargo-deny
checks advisories across runtime and development dependencies. Weekly
Dependabot PRs update SHA-pinned actions and Cargo dependencies; Cargo.lock is
committed, and CI uses `--locked`.
