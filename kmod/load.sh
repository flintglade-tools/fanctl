#!/usr/bin/env bash
# Run as root (by nct6687-rs.service at boot): builds, signs and installs nct6687_rs for the
# running kernel when it is missing (or with --rebuild), then loads it in place of nct6683.
set -euo pipefail
here=$(cd "$(dirname "$0")" && pwd)
kver=$(uname -r)
ko=/lib/modules/$kver/extra/nct6687_rs.ko
key=/etc/pki/akmods/private/private_key.priv
cert=/etc/pki/akmods/certs/public_key.der
if [[ ${1:-} == --rebuild || ! -f $ko ]]; then
    built=$(NCT_TOOLCHAIN=/var/cache/nct6687-rs/toolchain NCT_OUT=/var/cache/nct6687-rs/build "$here/build.sh" "$kver" | tail -1)
    [[ -r $key && -r $cert ]] || { echo "akmods signing key missing ($key, $cert)" >&2; exit 1; }
    "/usr/src/kernels/$kver/scripts/sign-file" sha256 "$key" "$cert" "$built"
    install -D -m 0644 "$built" "$ko"
    command -v restorecon >/dev/null && restorecon "$ko"
    depmod -a "$kver"
fi
if [[ -d /sys/module/nct6683 ]]; then modprobe -r nct6683; fi
modprobe nct6687_rs
