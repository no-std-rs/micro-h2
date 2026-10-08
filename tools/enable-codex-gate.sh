#!/usr/bin/env bash
# Require the existing Runnerless check without replacing other protection settings.
set -euo pipefail

repo=no-std-rs/micro-h2
context='Runnerless / Codex review'
app_id=4602759
pr_number=${1:?Usage: bash tools/enable-codex-gate.sh PR_NUMBER}
[[ "$pr_number" =~ ^[1-9][0-9]*$ ]]
command -v gh >/dev/null
command -v jq >/dev/null

pr=$(gh api "repos/$repo/pulls/$pr_number")
test "$(jq -r .state <<< "$pr")" = open
test "$(jq -r .base.ref <<< "$pr")" = main
head_sha=$(jq -r .head.sha <<< "$pr")
gh api --paginate "repos/$repo/commits/$head_sha/check-runs?per_page=100" |
  jq -s -e --arg context "$context" --argjson app_id "$app_id" \
    'any(.[].check_runs[]; .name == $context and .app.id == $app_id)' > /dev/null || {
      echo "Runnerless has not reported the review check on PR #$pr_number. Deploy forwarding and verify App access first." >&2
      exit 1
    }

protection_file=$(mktemp)
trap 'rm -f "$protection_file"' EXIT
gh api "repos/$repo/branches/main/protection/required_status_checks" |
  jq --arg context "$context" --argjson app_id "$app_id" \
    '{strict, checks: ([.checks[] | {context, app_id} | select(.context != $context)] + [{context: $context, app_id: $app_id}])}' \
    > "$protection_file"
gh api --method PATCH "repos/$repo/branches/main/protection/required_status_checks" \
  --input "$protection_file" > /dev/null
gh api "repos/$repo/branches/main/protection/required_status_checks" |
  jq -e --arg context "$context" --argjson app_id "$app_id" \
    'any(.checks[]; .context == $context and .app_id == $app_id)' > /dev/null
echo "Required $context from App $app_id on $repo/main; existing CI checks and bypass settings preserved."
