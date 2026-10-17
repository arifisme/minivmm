#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
WORK="${WORK:-$ROOT/guest/build/runtimes}"
OUT="${OUT:-$ROOT/guest/out/runtimes}"
SUITE="${SUITE:-noble}"
MIRROR="${MIRROR:-http://archive.ubuntu.com/ubuntu}"
RUNTIMES="${RUNTIMES:-native python}"
AGENT="${AGENT:-}"

if [[ $EUID -ne 0 ]]; then
    echo "build-runtimes.sh must run as root (debootstrap and file ownership need it)" >&2
    exit 1
fi

mkdir -p "$WORK" "$OUT"

if [[ ! -f "$ROOT/guest/out/bzImage" ]]; then
    "$ROOT/scripts/build-guest.sh"
fi

if [[ -z "$AGENT" ]]; then
    cargo build --release --manifest-path "$ROOT/Cargo.toml" -p mv-agent --target x86_64-unknown-linux-musl
    AGENT="$ROOT/target/x86_64-unknown-linux-musl/release/mv-agent"
fi
if ! file "$AGENT" | grep -q 'statically linked\|static-pie linked'; then
    echo "$AGENT must be a static binary" >&2
    exit 1
fi

install_agent() {
    local root="$1"
    mkdir -p "$root"/{proc,sys,dev,tmp,run,sbin}
    install -m 0755 "$AGENT" "$root/sbin/mv-agent"
}

make_image() {
    local name="$1" root="$2"
    local used size
    used="$(du -sm "$root" | cut -f1)"
    size=$(( used + used / 3 + 16 ))
    rm -f "$OUT/$name.ext4"
    mkfs.ext4 -q -F -L "$name" -m 0 -E root_owner=0:0 -d "$root" "$OUT/$name.ext4" "${size}M"
    echo "built $OUT/$name.ext4 (${size} MiB)"
}

build_native() {
    local busybox="$WORK/busybox"
    if [[ ! -x "$busybox/bin/busybox" ]]; then
        rm -rf "$busybox" && mkdir -p "$busybox"
        (cd "$busybox" && apt-get download busybox-static >/dev/null 2>&1)
        dpkg-deb -x "$busybox"/busybox-static_*.deb "$busybox/pkg"
        mkdir -p "$busybox/bin"
        cp "$(find "$busybox/pkg" -name busybox -type f | head -n1)" "$busybox/bin/busybox"
    fi
    local root="$WORK/native-root"
    rm -rf "$root"
    mkdir -p "$root"/{bin,sbin,usr/bin,usr/sbin,etc,root}
    cp "$busybox/bin/busybox" "$root/bin/busybox"
    for applet in $("$busybox/bin/busybox" --list-full); do
        [[ -e "$root/$applet" ]] || ln -s /bin/busybox "$root/$applet"
    done
    echo 'root:x:0:0:root:/root:/bin/sh' > "$root/etc/passwd"
    echo 'root:x:0:' > "$root/etc/group"
    install_agent "$root"
    make_image native "$root"
}

build_python() {
    local base="$WORK/python-base"
    if [[ ! -x "$base/usr/bin/python3" ]]; then
        rm -rf "$base"
        debootstrap --variant=minbase --include=python3 "$SUITE" "$base" "$MIRROR"
        rm -rf "$base"/usr/share/{doc,man,info,locale}/* "$base"/var/cache/apt/* \
            "$base"/var/lib/apt/lists/* "$base"/var/log/*
    fi
    local root="$WORK/python-root"
    rm -rf "$root"
    cp -a "$base" "$root"
    install_agent "$root"
    make_image python "$root"
}

for runtime in $RUNTIMES; do
    case "$runtime" in
        native) build_native ;;
        python) build_python ;;
        *) echo "unknown runtime $runtime" >&2; exit 1 ;;
    esac
done
ls -l "$OUT"
