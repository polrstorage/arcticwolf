# Builds a Linux kernel Image (arm64) with the in-kernel NFS client enabled.
#
# Apple's `container` runtime ships a minimal kernel that has
# `CONFIG_NFS_FS` disabled, so a client container cannot `mount -t nfs`.
# We rebuild the *same* kernel version from Apple's own extracted config
# (apple-container.config) with the NFS client compiled in (=y, since the
# config has CONFIG_MODULES disabled).
#
# The resulting arch/arm64/boot/Image is what `container run --kernel`
# expects (the default kernel is itself an arm64 boot Image).

FROM debian:bookworm AS build

ARG KERNEL_VERSION=6.18.15

RUN apt-get update && apt-get install -y --no-install-recommends \
        build-essential \
        bc bison flex \
        libssl-dev libelf-dev \
        curl ca-certificates xz-utils \
        cpio kmod \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /build

RUN curl -fL -o linux.tar.xz \
        "https://cdn.kernel.org/pub/linux/kernel/v6.x/linux-${KERNEL_VERSION}.tar.xz" \
    && tar -xf linux.tar.xz \
    && rm linux.tar.xz

WORKDIR /build/linux-${KERNEL_VERSION}

# Start from Apple's container kernel config so the virtio/vsock plumbing
# matches what the Virtualization.framework guest expects.
COPY apple-container.config .config

# Enable the in-kernel NFS v3 client. CONFIG_MODULES is off in the base
# config, so everything must be built in (=y). olddefconfig then resolves
# the selected dependencies (SUNRPC, LOCKD, NFS_COMMON, ...).
RUN scripts/config --file .config \
        -e NETWORK_FILESYSTEMS \
        -e SUNRPC \
        -e NFS_FS \
        -e NFS_V2 \
        -e NFS_V3 \
        -e NFS_V3_ACL \
        -d NFS_V4 \
    && make olddefconfig \
    && grep -E "CONFIG_NFS_FS|CONFIG_NFS_V3|CONFIG_SUNRPC" .config

RUN make -j"$(nproc)" Image \
    && cp arch/arm64/boot/Image /build/Image \
    && ls -la /build/Image

# Minimal final stage so the Image can be copied out cheaply.
FROM scratch AS artifact
COPY --from=build /build/Image /Image
