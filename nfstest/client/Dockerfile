# NFS client image for the Apple-container integration test.
#
# Runs upstream nfstest's `nfstest_posix` against the Arctic Wolf server.
# Built natively for arm64 so it needs no emulation. The in-kernel NFS
# client is provided at run time via `container run --kernel` (Apple's
# default kernel has CONFIG_NFS_FS disabled — see nfstest/kernel/).
#
#   container build -t arcticwolf-nfstest:client -f nfstest/client/Dockerfile nfstest/client

FROM debian:bookworm-slim

# nfs-common: mount.nfs + rpc helpers. python3: nfstest is a Python suite.
# procps/iproute2: nfstest_posix probes the environment during setup.
RUN apt-get update && apt-get install -y --no-install-recommends \
        nfs-common \
        netbase \
        python3 \
        python3-pip \
        git \
        ca-certificates \
        iproute2 \
        procps \
        tcpdump \
        sudo \
    && rm -rf /var/lib/apt/lists/*

# Install nfstest from its git repository (same source the QEMU VM used).
RUN git clone --depth 1 git://git.linux-nfs.org/projects/mora/nfstest.git /opt/nfstest

ENV PYTHONPATH=/opt/nfstest
ENV PATH=/opt/nfstest/test:/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin

RUN mkdir -p /mnt/nfstest

ENTRYPOINT ["/bin/sh", "-c"]
