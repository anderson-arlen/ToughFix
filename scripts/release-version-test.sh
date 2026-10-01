#!/usr/bin/env bash
# SPDX-License-Identifier: MIT
set -euo pipefail
root=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)

check() {
    local versions
    versions=$(bash "$root/scripts/release-version.sh" "$1")
    [[ $versions == "$2"$'\n'"$3" ]]
}
reject() {
    if bash "$root/scripts/release-version.sh" "$1" >/dev/null 2>&1; then
        echo "unexpectedly accepted release tag: $1" >&2
        return 1
    fi
}
check v0.1.0 0.1.0 0.1.0
check v0.1.0-alpha.1 0.1.0-alpha.1 0.1.0alpha1
check v0.1.0-beta.2 0.1.0-beta.2 0.1.0beta2
check v0.1.0-rc.3 0.1.0-rc.3 0.1.0rc3
check v12.34.56 12.34.56 12.34.56
for tag in 0.1.0 v0.1 v0.1.0-rc1 v0.1.0-preview.1 v01.1.0 v1.0.0-rc.01 'v1.0.0;false'; do
    reject "$tag"
done
if command -v vercmp >/dev/null 2>&1; then
    [[ $(vercmp 0.1.0alpha1 0.1.0beta1) -lt 0 ]]
    [[ $(vercmp 0.1.0beta1 0.1.0rc1) -lt 0 ]]
    [[ $(vercmp 0.1.0rc1 0.1.0) -lt 0 ]]
fi
echo 'Release version checks passed'
