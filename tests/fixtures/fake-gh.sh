#!/bin/sh
# A stand-in for the GitHub CLI used by powerqueue's PR watcher tests.
#
# `gh api graphql ... -F number=<n>` prints $FAKE_GH_DIR/pr-<n>.json (the
# GraphQL response) and appends the call to $FAKE_GH_DIR/gh-calls.log; a
# missing file is an error like an unknown PR. `--version` and `auth status`
# succeed.
set -u
dir="${FAKE_GH_DIR:?FAKE_GH_DIR is not set}"
case "${1:-}" in
  --version) echo "gh version 9.9.9 (fake)"; exit 0 ;;
  auth) echo "Logged in to github.com (fake)" >&2; exit 0 ;;
  api) ;;
  pr)
    # `gh pr list --head <branch> ...`: answer $FAKE_GH_DIR/pr-list.json
    # (an empty list when it does not exist).
    head=""
    prev=""
    for arg in "$@"; do
      [ "$prev" = "--head" ] && head="$arg"
      prev="$arg"
    done
    echo "pr list head=$head" >> "$dir/gh-calls.log"
    if [ -f "$dir/pr-list.json" ]; then cat "$dir/pr-list.json"; else echo "[]"; fi
    exit 0 ;;
  *) echo "fake-gh: unsupported command: $*" >&2; exit 2 ;;
esac
number=""
for arg in "$@"; do
  case "$arg" in number=*) number="${arg#number=}" ;; esac
done
echo "api graphql number=$number" >> "$dir/gh-calls.log"
file="$dir/pr-$number.json"
if [ ! -f "$file" ]; then
  echo "fake-gh: Could not resolve to a PullRequest with the number of $number." >&2
  exit 1
fi
cat "$file"
