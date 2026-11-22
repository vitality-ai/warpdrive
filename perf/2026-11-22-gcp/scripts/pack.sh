#!/usr/bin/env bash
# pack.sh <repo-worktree> <commit> <out.tgz>: ship exactly a commit, with its id recorded.
set -e; R=$1; C=$(git -C "$R" rev-parse "$2"); T=$(mktemp -d)
git -C "$R" archive "$C" | tar x -C "$T"; echo "$C" > "$T/COMMIT"
COPYFILE_DISABLE=1 tar --no-xattrs -czf "$3" -C "$T" .; rm -rf "$T"; echo "packed $C -> $3"
