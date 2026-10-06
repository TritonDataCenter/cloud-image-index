#!/bin/sh
#
# This Source Code Form is subject to the terms of the Mozilla Public
# License, v. 2.0. If a copy of the MPL was not distributed with this
# file, You can obtain one at https://mozilla.org/MPL/2.0/.
#
# Copyright 2026 Edgecast Cloud LLC.
#
# Run the per-vendor matrix jobs and the assemble job locally, with the
# same commands CI runs: one `vendor` job per vendor (in parallel, each
# logging to its own file), then one `assemble` job over the fragments.
#
# usage: scripts/run-matrix-locally.sh [site-dir] [work-dir]
#
#   site-dir  the published tree to update in place (default: site)
#   work-dir  scratch space for fragments and logs (default: target/matrix)
#
# Exits non-zero if any vendor job failed or assembly failed. Failed
# vendors keep their previous files in site-dir.

set -u

site=${1:-site}
work=${2:-target/matrix}

cargo build --quiet -p cloud-image-index-generate || exit 1
gen=${CARGO_TARGET_DIR:-target}/debug/cloud-image-index-generate
vendors=$("$gen" vendors) || {
	echo "cannot list vendors" >&2
	exit 1
}

rm -rf "$work"
mkdir -p "$work/fragments" "$work/logs"

pids=""
for vendor in $vendors; do
	"$gen" vendor "$vendor" --previous "$site" --out "$work/fragments/$vendor" \
	    >"$work/logs/$vendor.log" 2>&1 &
	pids="$pids $vendor:$!"
done

failed=""
for entry in $pids; do
	vendor=${entry%%:*}
	if wait "${entry#*:}"; then
		echo "ok      $vendor"
	else
		echo "FAILED  $vendor (see $work/logs/$vendor.log)"
		failed="$failed $vendor"
	fi
done

"$gen" assemble --site "$site" "$work"/fragments/*
assembled=$?

if [ -n "$failed" ] || [ "$assembled" -ne 0 ]; then
	echo "failed vendors:${failed:- none}; assemble exit status: $assembled"
	exit 1
fi
