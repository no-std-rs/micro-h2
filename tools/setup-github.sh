#!/usr/bin/env bash
# Create/configure the public repository and push the local main branch.
set -euo pipefail

repo=no-std-rs/micro-h2
cd "$(dirname "${BASH_SOURCE[0]}")/.."

command -v gh >/dev/null
gh auth status --hostname github.com
test "$(git branch --show-current)" = main
test -z "$(git status --porcelain)"

if gh repo view "$repo" --json isPrivate --jq .isPrivate > /dev/null 2>&1; then
  if [ "$(gh repo view "$repo" --json isPrivate --jq .isPrivate)" != false ]; then
    echo "Refusing to change the visibility of an existing private repository: $repo" >&2
    exit 1
  fi
else
  gh repo create "$repo" --public \
    --description 'A no_std, allocation-free, sans-I/O HTTP/2 client with bounded streams and HPACK'
fi

expected_remote="https://github.com/$repo.git"
if git remote get-url origin > /dev/null 2>&1; then
  test "$(git remote get-url origin)" = "$expected_remote"
else
  git remote add origin "$expected_remote"
fi
git push --set-upstream origin main

gh api --method PATCH "repos/$repo" \
  -f default_branch=main \
  -F has_issues=true \
  -F has_wiki=false \
  -F allow_squash_merge=true \
  -F allow_merge_commit=false \
  -F allow_rebase_merge=false \
  -F allow_update_branch=true \
  -F delete_branch_on_merge=true \
  -f squash_merge_commit_title=PR_TITLE \
  -f squash_merge_commit_message=PR_BODY > /dev/null

protection_file=$(mktemp)
trap 'rm -f "$protection_file"' EXIT
retain_codex_gate=false
if gh api "repos/$repo/branches/main/protection/required_status_checks" \
  --jq 'any(.checks[]; .context == "Runnerless / Codex review" and .app_id == 4602759)' \
  2>/dev/null | grep -qx true; then
  retain_codex_gate=true
fi
cat > "$protection_file" <<'JSON'
{
  "required_status_checks": {
    "strict": true,
    "contexts": ["Stable Rust", "Rust 1.88 / no_std", "Advisories", "Conventional Commits"]
  },
  "enforce_admins": false,
  "required_pull_request_reviews": {
    "dismiss_stale_reviews": true,
    "required_approving_review_count": 0
  },
  "restrictions": null,
  "required_linear_history": true,
  "required_conversation_resolution": true,
  "allow_force_pushes": false,
  "allow_deletions": false
}
JSON
if [ "$retain_codex_gate" = true ]; then
  command -v jq >/dev/null
  protection_json=$(jq \
    '.required_status_checks.checks = ([.required_status_checks.contexts[] | {context: ., app_id: -1}] + [{context: "Runnerless / Codex review", app_id: 4602759}]) | del(.required_status_checks.contexts)' \
    "$protection_file")
  printf '%s\n' "$protection_json" > "$protection_file"
fi
gh api --method PUT "repos/$repo/branches/main/protection" \
  --input "$protection_file" > /dev/null

gh repo edit "$repo" --add-topic rust --add-topic no-std \
  --add-topic http2 --add-topic hpack --add-topic embedded
echo "Created/configured https://github.com/$repo and pushed main."
echo 'Release automation remains disabled until its GitHub App and crates.io identity are configured.'
