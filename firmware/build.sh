#!/bin/sh
# Build the hello-world stand-in "firmware":
#   firmware/build/vmlinuz      - Alpine linux-virt aarch64 kernel
#   firmware/build/initramfs.gz - cpio.gz initramfs with busybox + helloware
#
# This whole directory (firmware/) is a placeholder. In production the launcher
# will consume a prebuilt artifact produced by the real Ark firmware repo, and
# this stand-in goes away entirely.
#
# Requires: docker (with linux/arm64 emulation), cargo + the
# aarch64-unknown-linux-musl target. The aarch64-musl build runs natively so we
# can use rust-lld and avoid pulling a full cross toolchain.

set -eu

FIRMWARE_DIR="$(cd "$(dirname "$0")" && pwd)"
BUILD="$FIRMWARE_DIR/build"
ALPINE_VERSION="3.23"

mkdir -p "$BUILD"

echo "[1/3] cross-compiling helloware (aarch64-musl)..."
( cd "$FIRMWARE_DIR/helloware" && cargo build --release )
HELLOWARE_BIN="$FIRMWARE_DIR/helloware/target/aarch64-unknown-linux-musl/release/helloware"
[ -x "$HELLOWARE_BIN" ] || { echo "helloware binary not found at $HELLOWARE_BIN"; exit 1; }

echo "[2/3] assembling initramfs + extracting kernel via Alpine $ALPINE_VERSION (arm64)..."
mkdir -p "$BUILD/stage"
cp "$HELLOWARE_BIN" "$BUILD/stage/helloware"

# Feed the inner build program to the container via stdin with a quoted-delim
# heredoc. This bypasses the host shell's quoting entirely -- nothing inside
# OUTER is expanded, so apostrophes, dollars, and backticks pass through verbatim.
docker run --rm -i --platform linux/arm64 \
    -v "$BUILD:/out" \
    alpine:"$ALPINE_VERSION" \
    sh -eu <<'OUTER'
    apk add --no-cache linux-virt busybox-static cpio > /dev/null

    cp /boot/vmlinuz-virt /out/vmlinuz

    ROOTFS=/tmp/rootfs
    for d in bin sbin etc proc sys dev tmp usr/bin run; do
        mkdir -p "$ROOTFS/$d"
    done

    cp /bin/busybox.static "$ROOTFS/bin/busybox"
    for cmd in sh mount umount echo cat ls poweroff ip modprobe insmod; do
        ln -sf busybox "$ROOTFS/bin/$cmd"
    done

    # Ship the kernel modules so modprobe can pull in virtio_net (+ its deps:
    # net_failover, failover). Alpine's linux-virt has VIRTIO_NET=m.
    mkdir -p "$ROOTFS/lib/modules"
    cp -a /lib/modules/* "$ROOTFS/lib/modules/"

    cp /out/stage/helloware "$ROOTFS/usr/bin/helloware"
    chmod +x "$ROOTFS/usr/bin/helloware"

    cat > "$ROOTFS/init" <<'INIT'
#!/bin/sh
export PATH=/usr/bin:/bin:/sbin
mount -t proc     proc     /proc
mount -t sysfs    sysfs    /sys
mount -t devtmpfs devtmpfs /dev

# virtio_net is shipped as a module in Alpine linux-virt; load it before
# trying to configure eth0.
modprobe virtio_net

# Bring up loopback + the guest NIC. QEMU SLIRP gives the guest 10.0.2.15/24
# with the host-side at 10.0.2.2; binding the WS server to 0.0.0.0:8080 then
# makes it reachable via the launchers hostfwd rule.
ip link set lo up
ip addr add 10.0.2.15/24 dev eth0
ip link set eth0 up

echo "[init] mounted pseudo-fs, network up, exec helloware..." > /dev/console
exec /usr/bin/helloware </dev/console >/dev/console 2>&1
INIT
    chmod +x "$ROOTFS/init"

    cd "$ROOTFS"
    find . -print0 | cpio --null -ov --format=newc 2>/dev/null | gzip -9 > /out/initramfs.gz
OUTER

rm -rf "$BUILD/stage"

echo "[3/3] artifacts:"
ls -lh "$BUILD/vmlinuz" "$BUILD/initramfs.gz"
echo "done."
