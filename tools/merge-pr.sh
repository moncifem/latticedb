#!/usr/bin/env bash
# Merge pull requests one at a time, safely. See docs/merging.md.
#
# usage: tools/merge-pr.sh [--dry-run] [--tries N] [--sleep SECONDS] PR [PR...]
#
# Every value read from gh is validated. An API error, an empty answer or an
# unrecognised value is UNKNOWN and is retried; it is never treated as a result.
set -u

DRY_RUN=0
TRIES=${MERGE_PR_TRIES:-90}
SLEEP=${MERGE_PR_SLEEP:-20}
PRS=""

usage() {
  echo "usage: ${0##*/} [--dry-run] [--tries N] [--sleep SECONDS] PR [PR...]" >&2
}

is_number() {
  case "$1" in
    '' | *[!0-9]*) return 1 ;;
    *) return 0 ;;
  esac
}

while [ $# -gt 0 ]; do
  case "$1" in
    --dry-run) DRY_RUN=1 ;;
    --tries)
      shift
      TRIES=${1:-}
      ;;
    --sleep)
      shift
      SLEEP=${1:-}
      ;;
    -h | --help)
      usage
      exit 0
      ;;
    -*)
      usage
      exit 2
      ;;
    *)
      is_number "$1" || {
        echo "not a PR number: $1" >&2
        exit 2
      }
      PRS="$PRS $1"
      ;;
  esac
  shift
done

is_number "$TRIES" && [ "$TRIES" -gt 0 ] || {
  echo "--tries needs a positive number" >&2
  exit 2
}
is_number "$SLEEP" || {
  echo "--sleep needs a number" >&2
  exit 2
}
[ -n "$PRS" ] || {
  usage
  exit 2
}

nap() {
  [ "$SLEEP" -eq 0 ] || sleep "$SLEEP"
}

# Derive the repository from the current checkout; retry because this is an API call.
REPO=""
i=0
while [ -z "$REPO" ] && [ "$i" -lt "$TRIES" ]; do
  REPO=$(gh repo view --json nameWithOwner --jq .nameWithOwner 2>/dev/null)
  case "$REPO" in
    */*) ;;
    *)
      REPO=""
      i=$((i + 1))
      nap
      ;;
  esac
done
[ -n "$REPO" ] || {
  echo "cannot determine the repository (gh repo view kept failing)" >&2
  exit 1
}

# view PR FIELD [JQ]: print the value, or nothing (UNKNOWN) when gh fails.
view() {
  gh pr view "$1" --repo "$REPO" --json "$2" --jq "${3:-.$2}" 2>/dev/null
}

FAIL_JQ='[.statusCheckRollup[]? | select(
  ((.conclusion // "") | IN("FAILURE", "TIMED_OUT", "STARTUP_FAILURE", "ACTION_REQUIRED"))
  or ((.state // "") | IN("FAILURE", "ERROR")))] | length'

# merge_one PR: prints exactly one outcome line. Returns 0 on merged, already merged or a
# dry-run plan; non-zero on refusal or when the retries ran out.
merge_one() {
  pr=$1
  stale_head=""
  updated_head=""
  attempt=0
  while [ "$attempt" -lt "$TRIES" ]; do
    attempt=$((attempt + 1))

    state=$(view "$pr" state)
    case "$state" in
      OPEN) ;;
      MERGED)
        echo "PR $pr: already merged"
        return 0
        ;;
      CLOSED)
        echo "PR $pr: refused, closed without merging"
        return 1
        ;;
      *)
        nap
        continue
        ;;
    esac

    head=$(view "$pr" headRefOid)
    ms=$(view "$pr" mergeStateStatus)
    fails=$(view "$pr" statusCheckRollup "$FAIL_JQ")
    head_again=$(view "$pr" headRefOid)
    if [ -z "$head" ] || [ "$head" != "$head_again" ] || [ -z "$ms" ] || ! is_number "$fails"; then
      nap
      continue
    fi

    if [ "$fails" -gt 0 ]; then
      echo "PR $pr: refused, $fails check(s) failed on head $head"
      return 1
    fi

    case "$ms" in
      DIRTY)
        echo "PR $pr: refused, merge conflicts"
        return 1
        ;;
      DRAFT)
        echo "PR $pr: refused, draft"
        return 1
        ;;
      BEHIND)
        if [ "$DRY_RUN" -eq 1 ]; then
          echo "PR $pr: dry run, would update the branch, wait for checks, then merge"
          return 0
        fi
        if [ "$head" != "$updated_head" ]; then
          if gh pr update-branch "$pr" --repo "$REPO" --rebase >/dev/null 2>&1 \
            || gh pr update-branch "$pr" --repo "$REPO" >/dev/null 2>&1; then
            updated_head=$head
            stale_head=$head
          fi
        fi
        nap
        ;;
      CLEAN)
        if [ "$head" = "$stale_head" ]; then
          # The state still describes the head from before the update; wait for the new one.
          nap
          continue
        fi
        if [ "$DRY_RUN" -eq 1 ]; then
          echo "PR $pr: dry run, would merge (rebase, delete branch)"
          return 0
        fi
        gh pr merge "$pr" --repo "$REPO" --rebase --delete-branch >/dev/null 2>&1
        # Trust only a fresh read, not the exit status of the merge call.
        if [ "$(view "$pr" state)" = MERGED ]; then
          echo "PR $pr: merged"
          return 0
        fi
        nap
        ;;
      *)
        # BLOCKED, UNSTABLE, HAS_HOOKS, UNKNOWN: checks pending or state not computed yet.
        if [ "$DRY_RUN" -eq 1 ]; then
          echo "PR $pr: dry run, would wait ($ms), then merge when CLEAN"
          return 0
        fi
        nap
        ;;
    esac
  done
  echo "PR $pr: gave up after $TRIES attempts without a verified result; not merged"
  return 1
}

status=0
for pr in $PRS; do
  if [ "$status" -ne 0 ]; then
    echo "PR $pr: skipped, an earlier PR was not merged"
    continue
  fi
  merge_one "$pr" || status=1
done
exit "$status"
