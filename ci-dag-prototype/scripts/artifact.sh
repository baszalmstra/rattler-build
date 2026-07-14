#!/usr/bin/env bash
# Artifact lookup/download with provenance verification.
#
# GitHub artifact names are unique only *within* a run, and any code running
# in any job can upload an artifact under ANY name (the runner's upload token
# is ambient - that is how upload-artifact itself works). A repo-wide lookup
# by name must therefore verify who produced the artifact, or a malicious
# fork PR could poison another sha's package namespace.
#
# Verification: the artifact's creating run must be the expected workflow
# file, and must match either
#   title=<display_title>  - for "Build Node" runs. run-name is set by the
#                            trusted workflow file (from dispatch inputs)
#                            before any untrusted step executes, and a run
#                            cannot rename itself, so the title is a
#                            trustworthy provenance carrier. head_sha is
#                            useless here: dispatched runs execute on the
#                            default branch ref, not the namespace sha.
#   head=<sha>             - for "Plan" runs, which execute on the real ref.
#
# usage: artifact.sh find     <name> <workflow-file> title=<t>|head=<sha>
#        artifact.sh download <name> <workflow-file> title=<t>|head=<sha> <dest-dir>
#
# `find` prints the artifact id of the newest verified match (empty + exit 0
# if none). `download` unzips the artifact into <dest-dir> (exit 1 if none).
set -euo pipefail

repo="${GITHUB_REPOSITORY:?GITHUB_REPOSITORY must be set}"

cmd="${1:?usage: artifact.sh find|download <name> <workflow-file> title=..|head=.. [dest]}"
name="${2:?artifact name}"
wf="${3:?workflow file, e.g. build-node.yml}"
match="${4:?title=<display_title> or head=<sha>}"
match_kind="${match%%=*}"
match_val="${match#*=}"

if [ "$match_kind" != "title" ] && [ "$match_kind" != "head" ]; then
  echo "artifact.sh: match must be title=... or head=..." >&2
  exit 2
fi

find_verified() {
  local pairs art_id run_id run_json
  pairs=$(gh api "repos/$repo/actions/artifacts?name=$name&per_page=50" \
    --jq '[.artifacts[] | select(.expired == false)] | sort_by(.created_at) | reverse | .[] | "\(.id)\t\(.workflow_run.id)"')
  while IFS=$'\t' read -r art_id run_id; do
    [ -n "$art_id" ] || continue
    run_json=$(gh api "repos/$repo/actions/runs/$run_id")
    if [ "$match_kind" = "title" ]; then
      if jq -e --arg wf ".github/workflows/$wf" --arg v "$match_val" \
        'select(.path == $wf and .display_title == $v)' <<<"$run_json" >/dev/null 2>&1; then
        echo "$art_id"
        return 0
      fi
    else
      if jq -e --arg wf ".github/workflows/$wf" --arg v "$match_val" \
        'select(.path == $wf and .head_sha == $v)' <<<"$run_json" >/dev/null 2>&1; then
        echo "$art_id"
        return 0
      fi
    fi
  done <<<"$pairs"
  return 1
}

case "$cmd" in
  find)
    find_verified || true
    ;;
  download)
    dest="${5:?destination dir}"
    art_id=$(find_verified) || {
      echo "artifact.sh: no verified artifact '$name' from $wf ($match)" >&2
      exit 1
    }
    mkdir -p "$dest"
    tmp=$(mktemp)
    gh api "repos/$repo/actions/artifacts/$art_id/zip" >"$tmp"
    unzip -o -q "$tmp" -d "$dest"
    rm -f "$tmp"
    echo "downloaded artifact $name (id $art_id) -> $dest" >&2
    ;;
  *)
    echo "artifact.sh: unknown command '$cmd'" >&2
    exit 2
    ;;
esac
