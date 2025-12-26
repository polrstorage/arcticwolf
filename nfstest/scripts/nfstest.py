#!/usr/bin/env python3
"""NFS test infrastructure management CLI."""

import argparse
import subprocess
import sys
import time
import shutil
from pathlib import Path

from config import Config, CONTAINER_NAME, NFS_PORT, QEMU_MEMORY, VM_SSH_PORT, VM_PASSWORD, RUNNER_SCRIPT, PROJECT_ROOT


def run_command(cmd, check=True, shell=False, cwd=None, silent=False, stream=False):
    """Run a command and return the result.

    Args:
        cmd: Command to run (list or string)
        check: Exit on error if True
        shell: Run in shell if True
        cwd: Working directory
        silent: If True, suppress all output except errors
        stream: If True, stream output directly to terminal (real-time), don't capture
    """
    if isinstance(cmd, str) and not shell:
        cmd = cmd.split()

    cmd_str = ' '.join(cmd) if isinstance(cmd, list) else cmd

    # Always print the command unless silent
    if not silent:
        print(f"$ {cmd_str}")

    # Stream mode: output goes directly to terminal
    if stream and not silent:
        result = subprocess.run(
            cmd,
            check=False,
            shell=shell,
            cwd=cwd
        )

        if check and result.returncode != 0:
            sys.exit(result.returncode)

        return result

    # Capture mode: capture output then print
    result = subprocess.run(
        cmd,
        check=False,
        shell=shell,
        cwd=cwd,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        text=True
    )

    # Print output unless silent
    if not silent:
        if result.stdout:
            print(result.stdout, end='')
        if result.stderr:
            print(result.stderr, end='', file=sys.stderr)

    if check and result.returncode != 0:
        if silent:
            # If silent mode, still show error
            print(f"✗ Error running command: {cmd_str}", file=sys.stderr)
            if result.stderr:
                print(f"stderr: {result.stderr}", file=sys.stderr)
            if result.stdout:
                print(f"stdout: {result.stdout}", file=sys.stderr)
        sys.exit(result.returncode)

    return result


def is_container_running(container_name):
    """Check if a Docker container is running."""
    print(f"Checking if container '{container_name}' is running...")
    result = run_command(
        ["docker", "ps", "--format", "{{.Names}}"],
        check=False,
        silent=True
    )
    is_running = container_name in result.stdout.splitlines()
    if is_running:
        print(f"✓ Container '{container_name}' is running")
    else:
        print(f"✗ Container '{container_name}' is not running")
    return is_running


def is_vm_running():
    """Check if the VM is running."""
    print("Checking if VM is running...")
    result = run_command(
        ["pgrep", "-f", "qemu-system-x86_64.*vm-test.qcow2"],
        check=False,
        silent=True
    )
    is_running = result.returncode == 0
    if is_running:
        print(f"✓ VM is running (PID: {result.stdout.strip()})")
    else:
        print("✗ VM is not running")
    return is_running


def wait_for_port(host, port, timeout=30, description="service"):
    """Wait for a port to be available."""
    print(f"Waiting for {description} on {host}:{port} (timeout: {timeout}s)...")

    for i in range(timeout):
        result = run_command(
            ["nc", "-z", host, str(port)],
            check=False,
            silent=True
        )
        if result.returncode == 0:
            print(f"✓ {description.capitalize()} is ready after {i+1}s")
            return True

        if (i + 1) % 5 == 0:
            print(f"  Still waiting... ({i+1}s elapsed)")

        time.sleep(1)

    print(f"✗ ERROR: {description} not available after {timeout}s", file=sys.stderr)
    return False


def start_server(cfg):
    """Start NFS server Docker container (assumes image is already built)."""
    print("=" * 60)
    print("Starting NFS Server")
    print("=" * 60)
    print()

    # Check if container already running
    print("[1/2] Checking container status...")
    if is_container_running(CONTAINER_NAME):
        print("⚠ NFS server container already running - skipping start")
        print()
        return 0

    # Start container
    print()
    print(f"[2/2] Starting container '{CONTAINER_NAME}'...")
    print(f"  Image: {cfg.docker_image}")
    print(f"  Port mapping: {NFS_PORT}:{NFS_PORT}")
    run_command([
        "docker", "run", "-d",
        "--name", CONTAINER_NAME,
        "-p", f"{NFS_PORT}:{NFS_PORT}",
        cfg.docker_image
    ])
    print(f"✓ NFS server container started in background")
    print()
    return 0


def stop_server():
    """Stop NFS server container."""
    print("=" * 60)
    print("Stopping NFS Server")
    print("=" * 60)
    print()

    print(f"[1/2] Stopping container '{CONTAINER_NAME}'...")
    result = run_command(["docker", "stop", CONTAINER_NAME], check=False, silent=True)
    if result.returncode == 0:
        print("✓ Container stopped")
    else:
        print("⚠ Container was not running or already stopped")

    print()
    print(f"[2/2] Removing container '{CONTAINER_NAME}'...")
    result = run_command(["docker", "rm", CONTAINER_NAME], check=False, silent=True)
    if result.returncode == 0:
        print("✓ Container removed")
    else:
        print("⚠ Container was already removed")

    print()
    return 0


def start_client(cfg):
    """Start Alpine VM for NFS client testing (assumes VM image is already built)."""
    print("=" * 60)
    print("Starting NFS Client VM")
    print("=" * 60)
    print()

    # Check if VM already running
    print("[1/4] Checking VM status...")
    if is_vm_running():
        print("⚠ VM already running - skipping start")
        print()
        return 0

    # Copy base image to test image
    print()
    print("[2/4] Preparing VM image...")
    print(f"  Source: {cfg.vm_image}")
    print(f"  Destination: {cfg.vm_test_image}")
    cfg.vm_dir.mkdir(parents=True, exist_ok=True)
    shutil.copy(cfg.vm_image, cfg.vm_test_image)
    print("✓ VM image copied")

    # Start VM in background
    print()
    print(f"[3/4] Starting VM with QEMU...")
    print(f"  Memory: {QEMU_MEMORY}")
    print(f"  SSH port: {VM_SSH_PORT}")
    print(f"  Log file: {cfg.vm_log}")

    qemu_cmd = [
        "qemu-system-x86_64",
        "-m", QEMU_MEMORY,
        "-nographic",
        "-drive", f"file={cfg.vm_test_image},format=qcow2",
        "-drive", f"file={cfg.cidata_iso},format=raw",
        "-netdev", f"user,id=net0,hostfwd=tcp::{VM_SSH_PORT}-:22",
        "-device", "virtio-net-pci,netdev=net0",
        "-serial", "mon:stdio"
    ]

    # Start QEMU in background
    with open(cfg.vm_log, "w") as log_file:
        process = subprocess.Popen(
            qemu_cmd,
            stdout=log_file,
            stderr=subprocess.STDOUT,
            cwd=PROJECT_ROOT
        )

    print(f"✓ VM started in background (PID: {process.pid})")
    print(f"  Monitor logs with: tail -f {cfg.vm_log}")

    # Wait for cloud-init to complete by monitoring the VM log
    print()
    print(f"[4/4] Waiting for cloud-init to complete...")
    print("  Monitoring VM console output for completion marker...")

    max_wait_time = 300  # 5 minutes
    start_time = time.time()
    cloud_init_done = False

    # Custom marker from user-data that signals provisioning is complete
    # This is echoed to console in the final runcmd step, which appears in QEMU serial output (vm.log)
    COMPLETION_MARKER = "NFSTEST_VM_READY"

    while time.time() - start_time < max_wait_time:
        if cfg.vm_log.exists():
            try:
                with open(cfg.vm_log, 'r') as f:
                    log_content = f.read()

                    # Check for our custom completion marker
                    if COMPLETION_MARKER in log_content:
                        cloud_init_done = True
                        print(f"✓ cloud-init completed successfully")
                        break
            except Exception:
                # Log file might be being written to, ignore and retry
                pass

        time.sleep(2)

    if not cloud_init_done:
        print("⚠ WARNING: cloud-init completion marker not found in logs within timeout")
        print("  Checking if SSH is available as fallback...")
        if wait_for_port("localhost", VM_SSH_PORT, description="VM SSH", timeout=30):
            print("✓ SSH is available, proceeding")
        else:
            print("✗ ERROR: VM does not appear to be ready", file=sys.stderr)
            print()
            return 1

    print()
    return 0


def stop_client():
    """Stop client VM."""
    print("=" * 60)
    print("Stopping NFS Client VM")
    print("=" * 60)
    print()

    print("Stopping QEMU process...")
    result = run_command(["pkill", "-f", "qemu-system-x86_64.*vm-test.qcow2"], check=False, silent=True)
    if result.returncode == 0:
        print("✓ VM stopped")
    else:
        print("⚠ VM was not running or already stopped")

    print()
    return 0


def run_tests(cfg, testcase="open,read,write"):
    """Run NFS integration tests."""
    print("=" * 60)
    print("Arctic Wolf NFS Test")
    print("=" * 60)
    print()

    # Check sshpass
    print("[Preflight] Checking dependencies...")
    if not shutil.which("sshpass"):
        print("✗ ERROR: sshpass not installed. Install with: brew install sshpass", file=sys.stderr)
        return 1
    print("✓ sshpass is available")

    # Check if NFS server container is running
    print()
    print("[Step 1/4] Verifying NFS server is running...")
    if not is_container_running(CONTAINER_NAME):
        print("✗ ERROR: NFS server container is not running. Run 'make start-server' first.", file=sys.stderr)
        return 1
    print("✓ Using existing NFS server container")

    # Wait for NFS server to be ready
    print()
    print("[Step 2/4] Waiting for NFS server to be ready...")
    if not wait_for_port("localhost", NFS_PORT, description="NFS server"):
        # Show container logs on failure
        print()
        print("Showing container logs:")
        run_command(["docker", "logs", CONTAINER_NAME], check=False)
        return 1

    # Check if VM is running
    print()
    print("[Step 3/4] Verifying client VM is running...")
    if not is_vm_running():
        print("✗ ERROR: Client VM is not running. Run 'make start-client' first.", file=sys.stderr)
        return 1
    print("✓ Using existing client VM (cloud-init already completed)")

    # Copy runner script to VM
    print()
    print("[Step 4/4] Copying test script to VM...")
    print(f"  Source: {RUNNER_SCRIPT}")
    print(f"  Destination: root@localhost:/tmp/runner.py")
    scp_cmd = [
        "sshpass", "-p", VM_PASSWORD,
        "scp",
        "-o", "StrictHostKeyChecking=no",
        "-o", "UserKnownHostsFile=/dev/null",
        "-P", str(VM_SSH_PORT),
        str(RUNNER_SCRIPT),
        "root@localhost:/tmp/runner.py"
    ]
    run_command(scp_cmd)
    print("✓ Test script copied successfully")

    # Run tests
    print()
    print("=" * 60)
    print("Running tests in VM...")
    print("=" * 60)
    print()
    ssh_cmd = [
        "sshpass", "-p", VM_PASSWORD,
        "ssh",
        "-o", "StrictHostKeyChecking=no",
        "-o", "UserKnownHostsFile=/dev/null",
        "-p", str(VM_SSH_PORT),
        "root@localhost",
        f"python3 /tmp/runner.py --testcase {testcase}"
    ]
    # Run the test script - stream output directly to terminal in real-time
    result = run_command(ssh_cmd, check=False, stream=True)

    # Show test result
    print()
    if result.returncode == 0:
        print("NFS Test PASSED ✓")
    else:
        print(f"NFS Test FAILED ✗ (exit code: {result.returncode})")

    print()
    return result.returncode


def main():
    """Main entry point."""
    parser = argparse.ArgumentParser(
        description="NFS test infrastructure management",
        formatter_class=argparse.RawDescriptionHelpFormatter
    )

    subparsers = parser.add_subparsers(dest="command", help="Command to run")

    # Create a parent parser for common arguments
    # Defaults are defined in Makefile - these are only used when called directly
    parent_parser = argparse.ArgumentParser(add_help=False)
    parent_parser.add_argument("--image-name", required=True, help="Docker image name")
    parent_parser.add_argument("--image-tag", required=True, help="Docker image tag")
    parent_parser.add_argument("--vm-dir", required=True, help="VM output directory")
    parent_parser.add_argument("--vm-image", required=True, help="VM image filename")
    parent_parser.add_argument("--cidata", required=True, help="Cloud-init ISO filename")

    # Server commands
    subparsers.add_parser("start-server", parents=[parent_parser], help="Start NFS server Docker container")
    subparsers.add_parser("stop-server", help="Stop NFS server Docker container")

    # Client commands
    subparsers.add_parser("start-client", parents=[parent_parser], help="Start Alpine VM for NFS client testing")
    subparsers.add_parser("stop-client", help="Stop client VM")

    # Environment commands
    subparsers.add_parser("start-env", parents=[parent_parser], help="Start both server and client VM")
    subparsers.add_parser("stop-env", help="Stop both server and client VM")

    # Test command
    test_parser = subparsers.add_parser("test", parents=[parent_parser], help="Run NFS integration tests")
    test_parser.add_argument("--testcase", required=True, help="Test cases to run (comma-separated)")

    args = parser.parse_args()

    if not args.command:
        parser.print_help()
        return 1

    # Create configuration from command line arguments
    # All defaults must be provided by caller (typically Makefile)
    cfg = Config(
        image_name=args.image_name,
        image_tag=args.image_tag,
        vm_dir=args.vm_dir,
        vm_image=args.vm_image,
        cidata=args.cidata
    )

    # Execute command
    if args.command == "start-server":
        return start_server(cfg)
    elif args.command == "stop-server":
        return stop_server()
    elif args.command == "start-client":
        return start_client(cfg)
    elif args.command == "stop-client":
        return stop_client()
    elif args.command == "start-env":
        ret = start_server(cfg)
        if ret != 0:
            return ret
        return start_client(cfg)
    elif args.command == "stop-env":
        stop_server()
        stop_client()
        return 0
    elif args.command == "test":
        return run_tests(cfg, testcase=args.testcase)
    else:
        print(f"Unknown command: {args.command}", file=sys.stderr)
        return 1


if __name__ == "__main__":
    sys.exit(main())
