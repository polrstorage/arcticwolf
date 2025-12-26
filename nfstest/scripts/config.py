#!/usr/bin/env python3
"""Shared configuration for NFS testing infrastructure."""

from pathlib import Path

# Get project root (two levels up from this script)
SCRIPT_DIR = Path(__file__).parent.resolve()
PROJECT_ROOT = SCRIPT_DIR.parent.parent

# Docker configuration
CONTAINER_NAME = "arcticwolf-server"
NFS_PORT = 4000

# VM configuration
QEMU_MEMORY = "512M"
VM_SSH_PORT = 2222
VM_PASSWORD = "nfstest"

# Script paths
RUNNER_SCRIPT = SCRIPT_DIR / "runner.py"


class Config:
    """Runtime configuration from command line arguments.

    Defaults are provided by Makefile variables, so all parameters should be passed explicitly.
    """

    def __init__(self, image_name, image_tag, vm_dir, vm_image, cidata):
        # Docker configuration
        self.image_name = image_name
        self.image_tag = image_tag
        self.docker_image = f"{self.image_name}:{self.image_tag}"

        # VM path configuration
        self.vm_dir = PROJECT_ROOT / vm_dir
        self.vm_image_name = vm_image
        self.cidata_name = cidata

        self.vm_image = self.vm_dir / self.vm_image_name
        self.vm_test_image = self.vm_dir / "vm-test.qcow2"
        self.cidata_iso = self.vm_dir / self.cidata_name
        self.vm_log = self.vm_dir / "vm.log"
