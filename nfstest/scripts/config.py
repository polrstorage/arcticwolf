#!/usr/bin/env python3
"""Shared configuration for the Apple-container NFS integration test.

The test runs two Apple `container` instances on the default bridge
(192.168.64.0/24): the Arctic Wolf server and an nfstest client. The client
mounts the server's /data export and runs upstream `nfstest_posix`.
"""

from pathlib import Path

# Project layout: this file is nfstest/scripts/config.py.
SCRIPT_DIR = Path(__file__).parent.resolve()
NFSTEST_DIR = SCRIPT_DIR.parent
PROJECT_ROOT = NFSTEST_DIR.parent

# Container / image names.
SERVER_CONTAINER = "arcticwolf-server"
CLIENT_CONTAINER = "arcticwolf-nfstest-client"
SERVER_IMAGE = "arcticwolf:test"
CLIENT_IMAGE = "arcticwolf-nfstest:client"

# Build inputs.
SERVER_DOCKERFILE = NFSTEST_DIR / "server" / "Dockerfile"
CLIENT_DOCKERFILE = NFSTEST_DIR / "client" / "Dockerfile"
CLIENT_CONTEXT = NFSTEST_DIR / "client"

# Custom NFS-enabled kernel for the client container (Apple's default
# kernel has CONFIG_NFS_FS disabled).
KERNEL_DIR = NFSTEST_DIR / "kernel"
KERNEL_DOCKERFILE = KERNEL_DIR / "Dockerfile"
KERNEL_IMAGE_OUT = KERNEL_DIR / "out"
# `container build -o type=local` writes per-platform subdirectories.
KERNEL_IMAGE = KERNEL_IMAGE_OUT / "linux_arm64" / "Image"

# Server export advertised to clients and ports it binds.
NFS_EXPORT = "/data"
NFS_PORT = 2049
PORTMAP_PORT = 111

# Mount point inside the client container.
CLIENT_MOUNT_POINT = "/mnt/nfstest"
