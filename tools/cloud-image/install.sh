#!/bin/bash
#
# This Source Code Form is subject to the terms of the Mozilla Public
# License, v. 2.0. If a copy of the MPL was not distributed with this
# file, You can obtain one at https://mozilla.org/MPL/2.0/.
#
# Copyright 2026 Edgecast Cloud LLC.
#
# Install cloud-image in a SmartOS global zone:
#
#   curl -fsSL https://github.com/TritonDataCenter/cloud-image-index/releases/latest/download/install.sh | bash
#
# Downloads the binary from the same release, checks it against the
# release's SHA256SUMS, installs it as /opt/tools/sbin/cloud-image and
# writes its man page, gzipped, to /opt/tools/man/man8 (both on the
# global zone's PATH and MANPATH). Running it again updates an existing
# install.
#
# CLOUD_IMAGE_RELEASE picks a release other than the latest (e.g.
# v0.9.0). CLOUD_IMAGE_PREFIX and CLOUD_IMAGE_BASE_URL change where it
# installs and downloads from, for testing.

# Everything runs from main, called on the last line, so a download cut
# short by `curl | bash` runs nothing.
main() {
    set -euo pipefail

    local repo=https://github.com/TritonDataCenter/cloud-image-index
    local release=${CLOUD_IMAGE_RELEASE:-latest}
    local base
    if [[ $release == latest ]]; then
        base=$repo/releases/latest/download
    else
        base=$repo/releases/download/$release
    fi
    base=${CLOUD_IMAGE_BASE_URL:-$base}
    local prefix=${CLOUD_IMAGE_PREFIX:-/opt/tools}
    local asset=cloud-image-x86_64-unknown-illumos

    [[ $(uname -s) == SunOS && $(uname -v) == joyent_* ]] ||
        fail "cloud-image runs on SmartOS"
    [[ $(zonename) == global ]] ||
        fail "install cloud-image in the global zone"

    # Global, not local: the EXIT trap runs after main has returned.
    tmp=$(mktemp -d)
    trap 'rm -rf "$tmp"' EXIT

    echo "Downloading cloud-image ($release) ..."
    curl -fsSL -o "$tmp/$asset" "$base/$asset"
    curl -fsSL -o "$tmp/SHA256SUMS" "$base/SHA256SUMS"
    local want got
    want=$(awk -v f="$asset" '$2 == f { print $1 }' "$tmp/SHA256SUMS")
    got=$(digest -a sha256 "$tmp/$asset")
    [[ -n $want && $got == "$want" ]] ||
        fail "$asset does not match the release's SHA256SUMS"

    # Replace the binary with a rename, so a running cloud-image keeps the
    # file it started from.
    mkdir -p "$prefix/sbin" "$prefix/man/man8"
    cp "$tmp/$asset" "$prefix/sbin/.cloud-image.new"
    chmod 0755 "$prefix/sbin/.cloud-image.new"
    mv -f "$prefix/sbin/.cloud-image.new" "$prefix/sbin/cloud-image"
    "$prefix/sbin/cloud-image" man --out "$prefix/man/man8"
    gzip -f "$prefix/man/man8/cloud-image.8"

    echo "Installed $("$prefix/sbin/cloud-image" --version) as" \
        "$prefix/sbin/cloud-image; see cloud-image(8)."
}

fail() {
    echo "install.sh: $*" >&2
    exit 1
}

main "$@"
