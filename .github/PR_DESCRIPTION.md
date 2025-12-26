# Add automated NFS integration testing infrastructure

Implements comprehensive NFS testing infrastructure with Docker and QEMU-based Alpine VM.

Resolves #[ISSUE_NUMBER]

## Overview

This PR adds automated integration testing for the Arctic Wolf NFSv3 server using industry-standard tools (nfstest_posix) in isolated, reproducible environments. The implementation provides a simple `make nfstest` command that tests NFS protocol compliance against a real Linux client.

## Design Solution

### Architecture

The design implements a dual-environment testing system where the NFS server runs in a Docker container and tests execute from a QEMU VM running Alpine Linux:

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
│  │  ┌──────────┐  ┌──────────┐  ┌──────────┐  ┌──────────┐            │    │
│  │  │ start-env│  │   test   │  │ stop-env │  │  Config  │            │    │
│  │  └────┬─────┘  └────┬─────┘  └────┬─────┘  └──────────┘            │    │
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
└───────────────────────────────────────────────────────────────────────────────┘
```

**Key Components:**
- **Docker container**: Runs NFS server (Rust binary) on port 4000
- **QEMU VM**: Alpine Linux client with nfstest_posix, provisioned via cloud-init
- **Makefile**: Test orchestration with configurable parameters (single source of truth)
- **Python CLI (nfstest.py)**: Infrastructure management
- **Earthly**: Hermetic, containerized builds for both Docker image and VM artifacts

### Data Flow

```
User: make nfstest TESTCASE=open,read,write
    │
    ▼
┌─────────────────────────────────────────────────────────────┐
│ 1. Makefile: nfstest target                                 │
│    ├─► stop-test-env (cleanup)                              │
│    └─► start-test-env (build & start)                       │
└────────────────────────────────────────────────────────────┘
    │
    ▼
┌─────────────────────────────────────────────────────────────┐
│ 2. Earthly Builds                                           │
│    ├─► earthly +server-docker → arcticwolf:latest           │
│    └─► earthly +client-vm → vm.qcow2 + cidata.iso           │
└─────────────────────────────────────────────────────────────┘
    │
    ▼
┌─────────────────────────────────────────────────────────────┐
│ 3. nfstest.py start-env                                     │
│    ├─► docker run arcticwolf:latest (port 4000)             │
│    ├─► qemu-system-x86_64 (Alpine VM with cloud-init)       │
│    └─► Wait for: NFSTEST_VM_READY marker                    │
└─────────────────────────────────────────────────────────────┘
    │
    ▼
┌─────────────────────────────────────────────────────────────┐
│ 4. nfstest.py test                                          │
│    ├─► SCP runner.py to VM                                  │
│    └─► SSH: python3 /tmp/runner.py --testcase=...           │
└─────────────────────────────────────────────────────────────┘
    │
    ▼
┌─────────────────────────────────────────────────────────────┐
│ 5. runner.py (inside VM)                                    │
│    ├─► Wait for NFS port (10.0.2.2:4000)                    │
│    ├─► nfstest_posix --runtest=open,read,write              │
│    │   └─► mount, test, unmount                             │
│    └─► Return exit code                                     │
└─────────────────────────────────────────────────────────────┘
    │
    ▼
Results: Exit code → SSH → nfstest.py → Makefile → User
         (0 = PASSED ✓, non-zero = FAILED ✗)
```

## How This Design Meets Requirements

### Functional Requirements

#### ✅ FR1: Automated Test Execution
**Requirement:** Simple command to run NFS v3 protocol compliance tests using nfstest_posix.

**Solution:**
- Single command: `make nfstest`
- Automatically builds, starts, and tests the NFS server
- Uses nfstest_posix from Linux NFS project (industry-standard tool)
- See: [Makefile:55-56](Makefile#L55-L56), [nfstest.py:305-389](nfstest/scripts/nfstest.py#L305-L389)

#### ✅ FR2: Test Case Selection
**Requirement:** Specify which NFS operations to test via command-line parameter.

**Solution:**
- `TESTCASE` Makefile variable with default `open,read,write`
- Supports single test: `make nfstest TESTCASE=open`
- Supports multiple: `make nfstest TESTCASE=read,write,commit`
- Parameter flows: Makefile → nfstest.py → runner.py → nfstest_posix
- See: [Makefile:13](Makefile#L13), [runner.py:47-76](nfstest/scripts/runner.py#L47-L76)

#### ✅ FR3: Isolated Test Environment
**Requirement:** Tests run in isolated, reproducible environments.

**Solution:**
- Docker container for server isolation
- QEMU VM for client isolation
- Fresh VM instance created for each test run (vm-test.qcow2 copied from base image)
- Cloud-init provisions VM identically every time
- See: [nfstest.py:206-211](nfstest/scripts/nfstest.py#L206-L211), [Earthfile:61-80](Earthfile#L61-L80)

#### ✅ FR4: Real-time Feedback
**Requirement:** Test output streamed in real-time, not buffered.

**Solution:**
- Python subprocess with `stream=True` for direct output
- `flush=True` on all print statements in runner.py
- Explicit `sys.stdout.flush()` before/after subprocess calls
- See: [nfstest.py:14-76](nfstest/scripts/nfstest.py#L14-L76), [runner.py:35-103](nfstest/scripts/runner.py#L35-L103)

#### ✅ FR5: CI/CD Integration
**Requirement:** Proper exit codes (0 for success, non-zero for failure).

**Solution:**
- Exit code propagation: nfstest_posix → runner.py → SSH → nfstest.py → Makefile → user
- All error paths return non-zero exit codes
- `make nfstest` returns 0 only if all tests pass
- See: [runner.py:95-104](nfstest/scripts/runner.py#L95-L104), [nfstest.py:378-388](nfstest/scripts/nfstest.py#L378-L388)

#### ✅ FR6: Environment Management
**Requirement:** Manually start, stop, and clean up test environment.

**Solution:**
- `make start-test-env` - Build and start infrastructure
- `make stop-test-env` - Stop server and VM
- `make clean` - Stop and remove all artifacts
- See: [Makefile:49-64](Makefile#L49-L64)

### Non-Functional Requirements

#### ✅ NFR1: Configuration
**Requirement:** All defaults in single location (Makefile), no hidden defaults.

**Solution:**
- All defaults defined in Makefile with `?=` operator
- Python scripts require all parameters via `required=True`
- Config class has no defaults, only receives parameters
- See: [Makefile:4-13](Makefile#L4-L13), [config.py:23-43](nfstest/scripts/config.py#L23-L43), [nfstest.py:403-410](nfstest/scripts/nfstest.py#L403-L410)

#### ✅ NFR2: Performance
**Requirement:** VM provisioning within 5 minutes with proper cloud-init detection.

**Solution:**
- Cloud-init completion detected via custom marker `NFSTEST_VM_READY`
- Marker echoed to QEMU serial console (vm.log) in final runcmd step
- nfstest.py monitors log file for marker (5-minute timeout)
- SSH fallback if marker not found
- See: [nfstest.py:252-281](nfstest/scripts/nfstest.py#L252-L281), [user-data:48](nfstest/vm/user-data#L48)

#### ✅ NFR3: Reproducibility
**Requirement:** Each test run uses fresh VM instance.

**Solution:**
- Base image `vm.qcow2` preserved as template
- Test instance `vm-test.qcow2` created fresh each run via file copy
- QEMU launches from test instance, not base image
- See: [nfstest.py:206-211](nfstest/scripts/nfstest.py#L206-L211)

#### ✅ NFR4: Usability
**Requirement:** Simple `make` commands with sensible defaults.

**Solution:**
- Primary command: `make nfstest` (3 words, runs everything)
- Defaults allow zero-parameter execution
- `make help` provides usage examples
- Clear, progressive output with checkmarks (✓) and cross marks (✗)
- See: [Makefile:21-34](Makefile#L21-L34)

## Configuration Architecture

**Single Source of Truth: Makefile**
```makefile
IMAGE_NAME ?= arcticwolf          # Docker image name
IMAGE_TAG ?= latest                # Docker image tag
VM_OUTPUT_DIR ?= build/nfstest/vm  # VM artifact directory
TESTCASE ?= open,read,write        # Default test cases
```

All defaults defined in Makefile → passed to Earthly → passed to Python → no hidden defaults.

**Parameter Flow:**
```
Makefile variables → Earthly ARGs → nfstest.py CLI args → Config class → Test execution
```

## Implementation Highlights

### 1. Cloud-init Completion Detection
Monitors QEMU serial console for custom marker `NFSTEST_VM_READY`, which is echoed from the final cloud-init runcmd step. This ensures tests don't start until VM provisioning is complete.

**Implementation:** [nfstest.py:252-281](nfstest/scripts/nfstest.py#L252-L281), [user-data:47-48](nfstest/vm/user-data#L47-L48)

### 2. Environment Variables for All Shell Types
Uses `/etc/environment` instead of `/etc/profile` to ensure PATH and PYTHONPATH are available in both login and non-login shells (critical for SSH command execution).

**Implementation:** [user-data:45-46](nfstest/vm/user-data#L45-L46)

### 3. Output Streaming
Real-time test feedback via `stream=True` in subprocess calls, with explicit flushing to maintain correct output ordering.

**Implementation:** [nfstest.py:14-76](nfstest/scripts/nfstest.py#L14-L76), [runner.py:72-99](nfstest/scripts/runner.py#L72-L99)

### 4. Exit Code Propagation
Proper failure signaling throughout the stack: nfstest_posix → subprocess → SSH → nfstest.py → Makefile

**Implementation:** [runner.py:95-104](nfstest/scripts/runner.py#L95-L104), [nfstest.py:378-388](nfstest/scripts/nfstest.py#L378-L388)

### 5. Network Topology
- Docker: Bridge network with port 4000 exposed
- QEMU: User networking where host appears as 10.0.2.2
- Port forwarding: localhost:2222 → VM:22 (SSH), VM → 10.0.2.2:4000 (NFS)

**Implementation:** [nfstest.py:220-229](nfstest/scripts/nfstest.py#L220-L229), [runner.py:15-16](nfstest/scripts/runner.py#L15-L16)

## Files Added

- [Earthfile](Earthfile) - Added `+server-docker` and `+client-vm` targets
- [Makefile](Makefile) - Added test orchestration targets (start-test-env, nfstest, stop-test-env, clean)
- [nfstest/scripts/nfstest.py](nfstest/scripts/nfstest.py) - Main CLI orchestration tool (469 lines)
- [nfstest/scripts/runner.py](nfstest/scripts/runner.py) - VM test execution script (109 lines)
- [nfstest/scripts/config.py](nfstest/scripts/config.py) - Configuration management (43 lines)
- [nfstest/vm/user-data](nfstest/vm/user-data) - Cloud-init provisioning config

## Usage Examples

```bash
# Run default tests (open, read, write)
make nfstest

# Run specific test
make nfstest TESTCASE=open

# Run multiple tests
make nfstest TESTCASE=read,write,commit

# Custom Docker image
make nfstest IMAGE_NAME=myserver IMAGE_TAG=dev

# Manual control
make start-test-env    # Build and start infrastructure
make stop-test-env     # Stop infrastructure
make clean             # Remove all artifacts
```

## Test Cases Supported

Using nfstest_posix from https://git.linux-nfs.org/projects/mora/nfstest.git:
- `open` - File open/close operations
- `read` - Read operations
- `write` - Write operations
- `commit` - NFS COMMIT operation
- `link` - Hard link operations
- `mknod` - Special file creation
- And more (see nfstest_posix documentation)

## Future Enhancements

- CI/CD integration (GitHub Actions)
- Additional test suites beyond nfstest_posix
- Performance benchmarking
- NFSv4 support
- Multi-client testing scenarios

---

🤖 Generated with [Claude Code](https://claude.com/claude-code)

Co-Authored-By: Claude Sonnet 4.5 <noreply@anthropic.com>
