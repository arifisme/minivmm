#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
WORK="${WORK:-$ROOT/guest/build}"
OUT="${OUT:-$ROOT/guest/out}"
KERNEL_VERSION="${KERNEL_VERSION:-v6.12}"
DROPBEAR_VERSION="${DROPBEAR_VERSION:-DROPBEAR_2024.86}"
ROOTFS_SIZE_MB="${ROOTFS_SIZE_MB:-64}"
JOBS="$(nproc)"

mkdir -p "$WORK" "$OUT"

build_kernel() {
    local src="$WORK/linux"
    if [[ ! -d "$src" ]]; then
        git clone -q -c advice.detachedHead=false --depth 1 --branch "$KERNEL_VERSION" https://github.com/torvalds/linux.git "$src"
    fi
    make -C "$src" -s tinyconfig
    "$src/scripts/kconfig/merge_config.sh" -m -O "$src" "$src/.config" "$ROOT/guest/kernel.config" >/dev/null
    make -C "$src" -s olddefconfig
    local missing=0
    while IFS= read -r line; do
        case "$line" in
            "# CONFIG_"*" is not set")
                local symbol="${line#\# }"
                symbol="${symbol% is not set}"
                ! grep -q "^$symbol=" "$src/.config"
                ;;
            *) grep -qx "$line" "$src/.config" ;;
        esac || { echo "kernel config option not applied: $line" >&2; missing=1; }
    done < "$ROOT/guest/kernel.config"
    [[ $missing -eq 0 ]]
    make -C "$src" -s -j"$JOBS" bzImage
    cp "$src/arch/x86/boot/bzImage" "$OUT/bzImage"
}

fetch_busybox() {
    local dir="$WORK/busybox"
    if [[ ! -x "$dir/bin/busybox" ]]; then
        rm -rf "$dir" && mkdir -p "$dir"
        (cd "$dir" && apt-get download busybox-static >/dev/null 2>&1)
        dpkg-deb -x "$dir"/busybox-static_*.deb "$dir/pkg"
        mkdir -p "$dir/bin"
        cp "$(find "$dir/pkg" -name busybox -type f | head -n1)" "$dir/bin/busybox"
    fi
    BUSYBOX="$dir/bin/busybox"
}

build_dropbear() {
    local src="$WORK/dropbear"
    if [[ ! -x "$src/dropbear" ]]; then
        rm -rf "$src"
        git clone -q -c advice.detachedHead=false --depth 1 --branch "$DROPBEAR_VERSION" https://github.com/mkj/dropbear.git "$src"
        (cd "$src" && ./configure --enable-static --disable-zlib --disable-pam \
            --disable-lastlog --disable-utmp --disable-utmpx --disable-wtmp --disable-wtmpx >/dev/null \
            && make -s -j"$JOBS" PROGRAMS="dropbear dropbearkey" STATIC=1)
    fi
    DROPBEAR="$src/dropbear"
}

install_busybox() {
    local root="$1"
    mkdir -p "$root"/{bin,sbin,usr/bin,usr/sbin,dev,proc,sys,tmp,etc,root}
    cp "$BUSYBOX" "$root/bin/busybox"
    for applet in $("$BUSYBOX" --list-full); do
        [[ -e "$root/$applet" ]] || ln -s /bin/busybox "$root/$applet"
    done
}

build_initramfs() {
    local root="$WORK/initramfs"
    rm -rf "$root"
    install_busybox "$root"
    cp "$ROOT/guest/initramfs-init" "$root/init"
    chmod 755 "$root/init"
    (cd "$root" && find . -print0 | cpio --null -o -H newc --owner=0:0 --quiet | gzip -9) > "$OUT/initramfs.cpio.gz"
}

build_rootfs() {
    local root="$WORK/rootfs"
    rm -rf "$root"
    install_busybox "$root"
    mkdir -p "$root"/{etc/init.d,etc/dropbear,usr/sbin,var/run,mnt}
    cp "$DROPBEAR" "$root/usr/sbin/dropbear"
    cp "$ROOT/guest/rcS" "$root/etc/init.d/rcS"
    cp "$ROOT/guest/inittab" "$root/etc/inittab"
    chmod 755 "$root/etc/init.d/rcS"
    echo 'root::0:0:root:/root:/bin/sh' > "$root/etc/passwd"
    echo 'root:x:0:' > "$root/etc/group"
    echo '/bin/sh' > "$root/etc/shells"
    rm -f "$OUT/rootfs.ext4"
    mkfs.ext4 -q -F -L rootfs -E root_owner=0:0 -d "$root" "$OUT/rootfs.ext4" "${ROOTFS_SIZE_MB}M"
}

build_kernel
fetch_busybox
build_dropbear
build_initramfs
build_rootfs
ls -l "$OUT"
