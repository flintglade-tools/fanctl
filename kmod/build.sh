#!/usr/bin/env bash
# Builds nct6687_rs.ko for a kernel (default: the running one) with the exact Fedora rustc that
# built that kernel, extracted privately into $NCT_TOOLCHAIN (nothing is installed system-wide).
# Prints the path of the built module.
set -euo pipefail
here=$(cd "$(dirname "$0")" && pwd)
kver=${1:-$(uname -r)}
kdir=/usr/src/kernels/$kver
[[ -d $kdir/rust ]] || { echo "kernel-devel for $kver (with Rust metadata) is not installed" >&2; exit 1; }
want=$(sed -n 's/^CONFIG_RUSTC_VERSION_TEXT="\(.*\)"$/\1/p' "$kdir/.config")
nvr=$(sed -n 's/.*(Fedora \([^)]*\))$/\1/p' <<<"$want")          # e.g. 1.97.1-1.fc44
[[ -n $nvr ]] || { echo "cannot parse the kernel's rustc version: $want" >&2; exit 1; }
tc=${NCT_TOOLCHAIN:-$here/../toolchain}/rust-$nvr
if [[ ! -x $tc/usr/bin/rustc ]]; then
    ver=${nvr%%-*} rel=${nvr#*-}
    rpm=$(mktemp --suffix=.rpm)
    archive=$(mktemp --suffix=.cpio)
    trap 'rm -f "$rpm" "$archive"' EXIT
    command -v nm-online >/dev/null && nm-online -q -t 120 || true
    curl -sSfL -o "$rpm" "https://kojipkgs.fedoraproject.org/packages/rust/$ver/$rel/x86_64/rust-$nvr.x86_64.rpm"
    # Unpack through a file: cpio stops reading at the archive trailer, which would end a pipe
    # from rpm2cpio with SIGPIPE and fail the script under pipefail.
    rpm2cpio "$rpm" > "$archive"
    mkdir -p "$tc"
    (cd "$tc" && cpio -idm --quiet < "$archive")
fi
got=$(LD_LIBRARY_PATH=$tc/usr/lib64 "$tc/usr/bin/rustc" --version)
[[ $got == "$want" ]] || { echo "rustc mismatch: kernel wants '$want', toolchain has '$got'" >&2; exit 1; }
out=${NCT_OUT:-$here/build}/$kver
mkdir -p "$out"
cp "$here/nct6687_rs.rs" "$here/Kbuild" "$out/"
rm -f "$out/nct6687_rs.ko"
LD_LIBRARY_PATH=$tc/usr/lib64 make -s -C "$kdir" M="$out" RUSTC="$tc/usr/bin/rustc" modules 2>&1 \
    | grep -v -E 'pahole|kernel was built with|You are using|Skipping BTF' >&2 || true
[[ ${PIPESTATUS[0]} -eq 0 && -f $out/nct6687_rs.ko ]] || { echo "build failed" >&2; exit 1; }
echo "$out/nct6687_rs.ko"
