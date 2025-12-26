# NFS Testing Infrastructure Design

## Architecture Overview

```
┌─────────────────────────────────────────────────────────────────────────────┐
│                              Host System (macOS)                             │
│                                                                               │
│  ┌─────────────────────────────────────────────────────────────────────┐    │
│  │                            Makefile                                  │    │
│  │  ┌──────────────┐  ┌──────────────┐  ┌────────────┐  ┌──────────┐  │    │
│  │  │ start-test-  │  │   nfstest    │  │ stop-test- │  │  clean   │  │    │
│  │  │     env      │  │              │  │    env     │  │          │  │    │
│  │  └──────┬───────┘  └──────┬───────┘  └──────┬─────┘  └────┬─────┘  │    │
│  └─────────┼──────────────────┼──────────────────┼───────────┼─────────┘    │
│            │                  │                  │           │               │
│            ▼                  ▼                  ▼           ▼               │
│  ┌─────────────────────────────────────────────────────────────────────┐    │
│  │                     nfstest.py (Python CLI)                          │    │
│  │  ┌──────────┐  ┌──────────┐  ┌──────────┐  ┌──────────┐  ┌──────┐  │    │
│  │  │ start-env│  │   test   │  │ stop-env │  │  Config  │  │ etc. │  │    │
│  │  └────┬─────┘  └────┬─────┘  └────┬─────┘  └──────────┘  └──────┘  │    │
│  └───────┼─────────────┼─────────────┼────────────────────────────────┘    │
│          │             │             │                                       │
│  ┌───────▼─────┐   ┌───▼─────┐   ┌──▼──────┐                               │
│  │   Earthly   │   │         │   │ Docker/ │                               │
│  │   Builds    │   │         │   │  QEMU   │                               │
│  └───────┬─────┘   │         │   │ Cleanup │                               │
│          │         │         │   └─────────┘                               │
│  ┌───────▼──────────────┐    │                                              │
│  │                      │    │                                              │
│  │  ┌────────────────┐  │    │      ┌─────────────────────────────────┐    │
│  │  │ Docker Image   │  │    │      │   QEMU VM (Alpine Linux)        │    │
│  │  ├────────────────┤  │    │      │  ┌───────────────────────────┐  │    │
│  │  │ NFS Server     │◄─┼────┼──────┼─►│  NFS Client               │  │    │
│  │  │ (Rust Binary)  │  │    │      │  │  ┌─────────────────────┐  │  │    │
│  │  │                │  │ NFS v3    │  │  │ runner.py           │  │  │    │
│  │  │ Port: 4000     │  │ TCP       │  │  │ ┌─────────────────┐ │  │  │    │
│  │  │                │  │           │  │  │ │ nfstest_posix   │ │  │  │    │
│  │  └────────────────┘  │           │  │  │ │ (POSIX tests)   │ │  │  │    │
│  │  Container:          │           │  │  │ └─────────────────┘ │  │  │    │
│  │  arcticwolf-server   │           │  │  └─────────────────────┘  │  │    │
│  └──────────────────────┘           │  │                            │  │    │
│                                      │  │  SSH: localhost:2222       │  │    │
│                                      │  │  NFS: 10.0.2.2:4000        │  │    │
│                                      │  └────────────────────────────┘  │    │
│                                      │  Provisioned via cloud-init      │    │
│                                      └──────────────────────────────────┘    │
│                                                                               │
│  ┌───────────────────────────────────────────────────────────────────────┐  │
│  │                          Build Artifacts                               │  │
│  │  build/nfstest/vm/                                                     │  │
│  │    ├── vm.qcow2          (Alpine base image)                           │  │
│  │    ├── vm-test.qcow2     (Test instance, created at runtime)           │  │
│  │    ├── cidata.iso        (Cloud-init NoCloud datasource)               │  │
│  │    └── vm.log            (QEMU console output)                         │  │
│  └───────────────────────────────────────────────────────────────────────┘  │
└───────────────────────────────────────────────────────────────────────────────┘
```

## Data Flow

```
User runs: make nfstest TESTCASE=open,read,write
    │
    ▼
┌────────────────────────────────────────────────────────────┐
│ 1. Makefile Target: nfstest                                │
│    ├─► Stop existing environment (stop-test-env)           │
│    └─► Start new environment (start-test-env)              │
└────────────────────────────────────────────────────────────┘
    │
    ▼
┌────────────────────────────────────────────────────────────┐
│ 2. Build Phase (Earthly)                                   │
│    ├─► earthly +server-docker → Docker image               │
│    └─► earthly +client-vm → VM artifacts                   │
└────────────────────────────────────────────────────────────┘
    │
    ▼
┌────────────────────────────────────────────────────────────┐
│ 3. Start Environment (nfstest.py start-env)                │
│    ├─► Launch Docker container (NFS server)                │
│    ├─► Copy base VM image → test instance                  │
│    ├─► Launch QEMU with cloud-init ISO                     │
│    └─► Wait for cloud-init marker: NFSTEST_VM_READY        │
└────────────────────────────────────────────────────────────┘
    │
    ▼
┌────────────────────────────────────────────────────────────┐
│ 4. Run Tests (nfstest.py test)                             │
│    ├─► Verify NFS server is running                        │
│    ├─► Verify VM is running                                │
│    ├─► Copy runner.py to VM via SCP                        │
│    └─► Execute via SSH: python3 /tmp/runner.py             │
└────────────────────────────────────────────────────────────┘
    │
    ▼
┌────────────────────────────────────────────────────────────┐
│ 5. Test Execution (Inside VM)                              │
│    ├─► Wait for NFS server port (10.0.2.2:4000)            │
│    ├─► Execute nfstest_posix with test cases               │
│    │   └─► Mount NFS export                                │
│    │   └─► Run POSIX compliance tests                      │
│    │   └─► Unmount and report results                      │
│    └─► Return exit code                                    │
└────────────────────────────────────────────────────────────┘
    │
    ▼
┌────────────────────────────────────────────────────────────┐
│ 6. Results Propagation                                     │
│    └─► Exit code flows back: runner.py → SSH → nfstest.py │
│        → Makefile → User (0=success, non-zero=failure)     │
└────────────────────────────────────────────────────────────┘
```

## Network Topology

```
┌──────────────────────────────────────────────────────────────┐
│  Host Network Stack                                          │
│                                                               │
│  ┌────────────────────────┐    ┌────────────────────────┐   │
│  │  Docker Bridge         │    │  QEMU User Networking  │   │
│  │  (docker0)             │    │  (slirp)               │   │
│  │                        │    │                        │   │
│  │  ┌──────────────────┐  │    │  ┌──────────────────┐  │   │
│  │  │  Container       │  │    │  │  VM              │  │   │
│  │  │  10.x.x.x        │  │    │  │  10.0.2.15       │  │   │
│  │  │                  │  │    │  │                  │  │   │
│  │  │  Port 4000       │◄─┼────┼──┤  → 10.0.2.2:4000 │  │   │
│  │  │  (NFS)           │  │    │  │  (Gateway=Host)  │  │   │
│  │  └──────────────────┘  │    │  │                  │  │   │
│  │         ▲              │    │  │  Port 22         │  │   │
│  │         │              │    │  │  (SSH)           │  │   │
│  │         │              │    │  └──────────────────┘  │   │
│  │  Published: 0.0.0.0:4000    │         ▲              │   │
│  └────────────────────────┘    │         │              │   │
│         ▲                      │  Forwarded: localhost:2222 │
│         │                      └────────────────────────┘   │
│         │                               ▲                   │
│    localhost:4000                  localhost:2222           │
└──────────────────────────────────────────────────────────────┘
```

## Configuration Flow

```
┌─────────────────────────────────────────────────────────────────┐
│  Makefile (Single Source of Truth for Defaults)                 │
│  ┌────────────────────────────────────────────────────────────┐ │
│  │  IMAGE_NAME ?= arcticwolf                                  │ │
│  │  IMAGE_TAG ?= latest                                       │ │
│  │  VM_OUTPUT_DIR ?= build/nfstest/vm                         │ │
│  │  VM_IMAGE_NAME ?= vm.qcow2                                 │ │
│  │  CIDATA_NAME ?= cidata.iso                                 │ │
│  │  TESTCASE ?= open,read,write                               │ │
│  └────────────────────────────────────────────────────────────┘ │
└────────────────┬────────────────────────────────────────────────┘
                 │
                 ▼
┌─────────────────────────────────────────────────────────────────┐
│  Earthly (Build-time Configuration)                             │
│  ┌────────────────────────────────────────────────────────────┐ │
│  │  ARG IMAGE_NAME=arcticwolf                                 │ │
│  │  ARG IMAGE_TAG=latest                                      │ │
│  │  ARG VM_OUTPUT_DIR=build/nfstest/vm                        │ │
│  │  ...                                                        │ │
│  │  SAVE IMAGE ${IMAGE_NAME}:${IMAGE_TAG}                     │ │
│  │  SAVE ARTIFACT vm.qcow2 AS LOCAL ${VM_OUTPUT_DIR}/...      │ │
│  └────────────────────────────────────────────────────────────┘ │
└─────────────────────────────────────────────────────────────────┘
                 │
                 ▼
┌─────────────────────────────────────────────────────────────────┐
│  nfstest.py CLI (Runtime Configuration)                         │
│  ┌────────────────────────────────────────────────────────────┐ │
│  │  --image-name (required)                                   │ │
│  │  --image-tag (required)                                    │ │
│  │  --vm-dir (required)                                       │ │
│  │  --vm-image (required)                                     │ │
│  │  --cidata (required)                                       │ │
│  │  --testcase (required for 'test' command)                  │ │
│  └────────────────────────────────────────────────────────────┘ │
└────────────────┬────────────────────────────────────────────────┘
                 │
                 ▼
┌─────────────────────────────────────────────────────────────────┐
│  Config Class (Internal Runtime State)                          │
│  ┌────────────────────────────────────────────────────────────┐ │
│  │  self.image_name                                           │ │
│  │  self.image_tag                                            │ │
│  │  self.docker_image = f"{image_name}:{image_tag}"           │ │
│  │  self.vm_dir = PROJECT_ROOT / vm_dir                       │ │
│  │  self.vm_image = self.vm_dir / vm_image                    │ │
│  │  self.vm_test_image = self.vm_dir / "vm-test.qcow2"        │ │
│  │  self.cidata_iso = self.vm_dir / cidata                    │ │
│  │  self.vm_log = self.vm_dir / "vm.log"                      │ │
│  └────────────────────────────────────────────────────────────┘ │
└─────────────────────────────────────────────────────────────────┘
```

## Component Interaction Sequence

```
User
  │
  │ make nfstest
  ▼
Makefile
  │
  │ calls: earthly +server-docker
  ▼
Earthly
  │
  │ builds: arcticwolf:latest Docker image
  ▼
Makefile
  │
  │ calls: earthly +client-vm
  ▼
Earthly
  │
  │ downloads: Alpine Linux image
  │ creates: cloud-init ISO
  │ outputs: build/nfstest/vm/{vm.qcow2, cidata.iso}
  ▼
Makefile
  │
  │ calls: nfstest.py start-env --image-name=... --vm-dir=...
  ▼
nfstest.py
  │
  │ docker run -d --name arcticwolf-server -p 4000:4000 arcticwolf:latest
  ▼
Docker
  │
  │ starts: NFS server container
  │ listening: 0.0.0.0:4000
  ▼
nfstest.py
  │
  │ cp build/nfstest/vm/vm.qcow2 → build/nfstest/vm/vm-test.qcow2
  │ qemu-system-x86_64 -m 512M -drive file=vm-test.qcow2 \
  │   -drive file=cidata.iso -netdev user,hostfwd=tcp::2222-:22 ...
  ▼
QEMU
  │
  │ boots: Alpine Linux VM
  │ runs: cloud-init (reads cidata.iso)
  ▼
cloud-init (inside VM)
  │
  │ installs: openssh, nfs-utils, python3, git
  │ clones: git://git.linux-nfs.org/projects/mora/nfstest.git → /opt/nfstest
  │ configures: /etc/environment (PATH, PYTHONPATH)
  │ signals: echo "NFSTEST_VM_READY" (to serial console)
  ▼
nfstest.py (monitoring vm.log)
  │
  │ detects: "NFSTEST_VM_READY" in vm.log
  │ proceeds: cloud-init complete
  ▼
Makefile
  │
  │ calls: nfstest.py test --testcase=open,read,write
  ▼
nfstest.py
  │
  │ verifies: Docker container running
  │ verifies: VM running
  │ scp: runner.py → root@localhost:2222:/tmp/runner.py
  │ ssh: root@localhost -p 2222 "python3 /tmp/runner.py --testcase=open,read,write"
  ▼
runner.py (inside VM)
  │
  │ waits: for NFS server (socket connect to 10.0.2.2:4000)
  │ executes: nfstest_posix --server 10.0.2.2 --port 4000 \
  │           --runtest=open,read,write --nfsversion 3
  ▼
nfstest_posix (inside VM)
  │
  │ mounts: mount -t nfs -o vers=3,port=4000 10.0.2.2:/ /mnt/nfstest
  │ runs: POSIX compliance tests
  │   ├─► open test
  │   ├─► read test
  │   └─► write test
  │ unmounts: umount /mnt/nfstest
  │ returns: exit code (0=pass, non-zero=fail)
  ▼
runner.py
  │
  │ captures: nfstest_posix exit code
  │ returns: same exit code
  ▼
nfstest.py
  │
  │ captures: SSH command exit code
  │ displays: "NFS Test PASSED ✓" or "NFS Test FAILED ✗"
  │ returns: exit code to Makefile
  ▼
Makefile
  │
  │ exits with: test result code
  ▼
User (sees result)
```

## File Organization

```
arcticwolf/
├── Earthfile                       # Build definitions
│   ├── +server-docker              # Builds NFS server Docker image
│   └── +client-vm                  # Builds Alpine VM with cloud-init
│
├── Makefile                        # Test orchestration
│   ├── start-test-env              # Build & start infrastructure
│   ├── nfstest                     # Run integration tests
│   ├── stop-test-env               # Stop infrastructure
│   └── clean                       # Remove all artifacts
│
├── nfstest/                        # Testing infrastructure
│   ├── scripts/
│   │   ├── config.py               # Configuration classes & constants
│   │   ├── nfstest.py              # Main CLI orchestration tool
│   │   └── runner.py               # VM test execution script
│   └── vm/
│       └── user-data               # Cloud-init configuration
│
└── build/nfstest/vm/               # Generated at build time
    ├── vm.qcow2                    # Alpine base image (downloaded)
    ├── vm-test.qcow2               # Test instance (copy of base)
    ├── cidata.iso                  # Cloud-init NoCloud datasource
    └── vm.log                      # QEMU console output
```

## Key Design Decisions

1. **Single Source of Truth**: All defaults in Makefile, no hidden defaults in Python
2. **Separation of Concerns**: Makefile (orchestration) → Python (implementation)
3. **Reproducible Builds**: Earthly for containerized, hermetic builds
4. **Cloud-init**: Industry-standard VM provisioning, declarative configuration
5. **Real-time Streaming**: Test output appears immediately, not buffered
6. **Exit Code Propagation**: Proper failure signaling for CI/CD integration
7. **Environment Isolation**: Each test run uses fresh VM instance (vm-test.qcow2)
