#!/usr/bin/env bash
# SPDX-License-Identifier: MIT
set -euo pipefail

if [[ $# -ne 1 ]]; then
    echo 'usage: release-version.sh <vMAJOR.MINOR.PATCH[-alpha.N|-beta.N|-rc.N]>' >&2
    exit 2
fi

tag=$1
if [[ ! $tag =~ ^v(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)(-(alpha|beta|rc)\.(0|[1-9][0-9]*))?$ ]]; then
    echo "invalid release tag: $tag" >&2
    exit 2
fi

display_version=${tag#v}
arch_version="${BASH_REMATCH[1]}.${BASH_REMATCH[2]}.${BASH_REMATCH[3]}"
if [[ -n ${BASH_REMATCH[5]:-} ]]; then
    arch_version+="${BASH_REMATCH[5]}${BASH_REMATCH[6]}"
fi
printf '%s\n%s\n' "$display_version" "$arch_version"
