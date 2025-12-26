# Add Automated NFS Integration Testing Infrastructure

## Problem Statement

The Arctic Wolf NFS v3 server currently lacks automated integration testing against real NFS clients. Manual testing is:
- Time-consuming and error-prone
- Unable to catch regressions effectively
- Not suitable for CI/CD pipelines
- Difficult to reproduce consistently

## Requirements

### Functional Requirements

#### FR1: Automated Test Execution
The system SHALL provide a simple command to run NFS v3 protocol compliance tests against the server using industry-standard testing tools (nfstest_posix).

#### FR2: Test Case Selection
Users SHALL be able to specify which NFS operations to test via a command-line parameter, supporting both single and multiple test cases.

#### FR3: Isolated Test Environment
Tests SHALL run in isolated, reproducible environments using containerization (Docker for server, QEMU VM for client).

#### FR4: Real-time Feedback
Test output SHALL be streamed in real-time to the console, not buffered.

#### FR5: CI/CD Integration
The system SHALL return proper exit codes (0 for success, non-zero for failure) to enable CI/CD integration.

#### FR6: Environment Management
Users SHALL be able to manually start, stop, and clean up the test environment independently of running tests.

### Non-Functional Requirements

#### NFR1: Configuration
All configuration defaults SHALL be defined in a single location (Makefile) with no hidden defaults elsewhere.

#### NFR2: Performance
VM provisioning SHALL complete within 5 minutes, with cloud-init completion properly detected.

#### NFR3: Reproducibility
Each test run SHALL use a fresh VM instance to ensure consistency.

#### NFR4: Usability
The primary interface SHALL be simple `make` commands with sensible defaults.

## User Interface Specification

### Primary Commands

#### 1. Run Tests (Primary Use Case)
```bash
make nfstest
```
**Behavior:**
- Stops any existing test environment
- Builds Docker image for NFS server
- Builds QEMU VM artifacts for NFS client
- Starts Docker container running NFS server on port 4000
- Starts QEMU VM with Alpine Linux and nfstest_posix
- Waits for cloud-init provisioning to complete
- Runs default test cases: `open`, `read`, `write`
- Streams test output in real-time
- Returns exit code 0 on success, non-zero on failure

**Default Configuration:**
- Docker image: `arcticwolf:latest`
- Test cases: `open,read,write`
- VM artifacts: `build/nfstest/vm/`

#### 2. Run Specific Test Cases
```bash
make nfstest TESTCASE=open
make nfstest TESTCASE=read,write,commit
make nfstest TESTCASE=link,mknod
```
**Behavior:**
- Same as `make nfstest` but runs only specified test cases
- Test cases are comma-separated with no spaces

#### 3. Custom Docker Image
```bash
make nfstest IMAGE_NAME=myserver IMAGE_TAG=dev
```
**Behavior:**
- Uses custom Docker image name and tag instead of defaults

#### 4. Manual Environment Control
```bash
make start-test-env   # Build and start infrastructure
make stop-test-env    # Stop all test infrastructure
make clean            # Stop and remove all artifacts
```

**`make start-test-env` behavior:**
- Builds Docker image: `arcticwolf:latest`
- Builds VM artifacts: `build/nfstest/vm/vm.qcow2`, `build/nfstest/vm/cidata.iso`
- Starts Docker container: `arcticwolf-server`
- Starts QEMU VM with cloud-init provisioning
- Waits for cloud-init completion marker: `NFSTEST_VM_READY`
- Exits when environment is ready

**`make stop-test-env` behavior:**
- Stops and removes Docker container: `arcticwolf-server`
- Terminates QEMU VM process

**`make clean` behavior:**
- Calls `make stop-test-env`
- Removes `target/` directory (Rust build artifacts)
- Removes `build/` directory (VM artifacts)

#### 5. Help
```bash
make help
```
**Output:**
```
Available targets:
  build           - Build release binary
  test            - Run unit tests
  lint            - Run clippy and rustfmt checks
  start-test-env  - Build and start both server and client VM
  nfstest         - Run NFS tests (TESTCASE=open,read,write)
  stop-test-env   - Stop both server and VM
  clean           - Stop all and remove build artifacts

Examples:
  make nfstest                    # Run default tests (open,read,write)
  make nfstest TESTCASE=open      # Run only open test
  make nfstest TESTCASE=read,write # Run read and write tests
```

### Configuration Parameters

All parameters are Makefile variables that can be overridden on the command line:

| Parameter | Default | Description |
|-----------|---------|-------------|
| `IMAGE_NAME` | `arcticwolf` | Docker image name for NFS server |
| `IMAGE_TAG` | `latest` | Docker image tag |
| `VM_OUTPUT_DIR` | `build/nfstest/vm` | Directory for VM artifacts |
| `VM_IMAGE_NAME` | `vm.qcow2` | VM base image filename |
| `CIDATA_NAME` | `cidata.iso` | Cloud-init ISO filename |
| `TESTCASE` | `open,read,write` | Test cases to run (comma-separated) |

### Supported Test Cases

The following test cases from `nfstest_posix` are supported:

| Test Case | Description |
|-----------|-------------|
| `open` | File open/close operations |
| `read` | File read operations |
| `write` | File write operations |
| `commit` | NFS COMMIT operation |
| `link` | Hard link operations |
| `mknod` | Special file creation (devices, FIFOs) |

Additional test cases available - see [nfstest_posix documentation](https://git.linux-nfs.org/projects/mora/nfstest.git)

### Output Format

**Test execution output:**
```
============================================================
Arctic Wolf NFS Test
============================================================

[Preflight] Checking dependencies...
✓ sshpass is available

[Step 1/4] Verifying NFS server is running...
✓ Using existing NFS server container

[Step 2/4] Waiting for NFS server to be ready...
✓ NFS server is ready after 1s

[Step 3/4] Verifying client VM is running...
✓ Using existing client VM (cloud-init already completed)

[Step 4/4] Copying test script to VM...
✓ Test script copied successfully

============================================================
Running tests in VM...
============================================================

NFS Server: 10.0.2.2:4000

Waiting for NFS server...
NFS server is reachable
Running nfstest_posix...

[... nfstest_posix output streamed in real-time ...]

Test Complete

NFS Test PASSED ✓
```

**Exit codes:**
- `0` - All tests passed
- `1` - Tests failed or infrastructure error

## System Architecture

### Components

```
┌─────────────────────────────────────────────────────────────┐
│                         Host System                          │
│  ┌────────────────┐              ┌─────────────────────┐    │
│  │ Docker         │              │ QEMU VM (Alpine)    │    │
│  │ ┌────────────┐ │              │ ┌─────────────────┐ │    │
│  │ │ NFS Server │ │◄─────────────┼─┤ NFS Client      │ │    │
│  │ │ (Rust)     │ │  NFS v3      │ │ (nfstest_posix) │ │    │
│  │ │ Port 4000  │ │  TCP         │ │                 │ │    │
│  │ └────────────┘ │              │ └─────────────────┘ │    │
│  └────────────────┘              └─────────────────────┘    │
│         ▲                                  ▲                 │
│         │                                  │                 │
│    ┌────┴──────────────────────────────────┴──────┐         │
│    │         Makefile + Python Scripts            │         │
│    └───────────────────────────────────────────────┘         │
└─────────────────────────────────────────────────────────────┘
```

**Network Configuration:**
- Docker container: Bridge network, port 4000 exposed
- QEMU VM: User networking, host accessible at 10.0.2.2
- SSH: localhost:2222 → VM:22 (for test script execution)
- NFS: VM connects to 10.0.2.2:4000

### Implementation Files

| File | Purpose |
|------|---------|
| `Makefile` | Primary user interface, orchestrates all operations |
| `Earthfile` | Build definitions for Docker image and VM artifacts |
| `nfstest/scripts/nfstest.py` | Infrastructure management CLI |
| `nfstest/scripts/runner.py` | Test execution script (runs inside VM) |
| `nfstest/scripts/config.py` | Configuration management |
| `nfstest/vm/user-data` | Cloud-init VM provisioning config |

## Acceptance Criteria

- [ ] User can run `make nfstest` and see test results
- [ ] User can specify test cases via `TESTCASE=open,read,write`
- [ ] User can override Docker image name and tag
- [ ] Test output is streamed in real-time, not buffered
- [ ] Exit code is 0 for passing tests, non-zero for failures
- [ ] `make help` shows all available commands
- [ ] `make clean` removes all artifacts and stops all processes
- [ ] Each test run uses a fresh VM instance
- [ ] Cloud-init completion is reliably detected before running tests
- [ ] All defaults are defined only in Makefile
- [ ] Documentation includes usage examples for all commands
