#!/usr/bin/env python3
"""NFS integration test orchestration using Apple `container`.

Replaces the previous Earthly + Docker + QEMU pipeline. Everything runs as
Apple containers:

  * the Arctic Wolf server (nfstest/server/Dockerfile)
  * an nfstest client (nfstest/client/Dockerfile) that mounts the server's
    /data export over the default 192.168.64.0/24 bridge and runs
    nfstest_posix

The client needs an in-kernel NFS client, which Apple's default kernel lacks,
so it boots with a custom kernel built by nfstest/kernel/Dockerfile.

Usage:
  nfstest.py build-images          # build server + client images
  nfstest.py build-kernel [--force]
  nfstest.py start-server          # run server detached, wait until ready
  nfstest.py run-test [--testcase read,write]
  nfstest.py stop                  # stop + remove server/client containers
  nfstest.py test [--testcase ...] # start-server + run-test + stop
"""

import argparse
import json
import subprocess
import sys
import time

from config import (
    SERVER_CONTAINER,
    CLIENT_CONTAINER,
    SERVER_IMAGE,
    CLIENT_IMAGE,
    SERVER_DOCKERFILE,
    CLIENT_DOCKERFILE,
    CLIENT_CONTEXT,
    KERNEL_DOCKERFILE,
    KERNEL_DIR,
    KERNEL_IMAGE_OUT,
    KERNEL_IMAGE,
    NFS_EXPORT,
    NFS_PORT,
    CLIENT_MOUNT_POINT,
    PROJECT_ROOT,
)

CONTAINER = "container"


def run(cmd, *, check=True, capture=False, stream=False, cwd=None):
    """Run a command, echoing it first."""
    print(f"$ {' '.join(cmd)}", flush=True)
    if stream:
        result = subprocess.run(cmd, check=False, cwd=cwd)
    elif capture:
        result = subprocess.run(
            cmd, check=False, cwd=cwd,
            stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True,
        )
    else:
        result = subprocess.run(cmd, check=False, cwd=cwd)
    if check and result.returncode != 0:
        if capture and result.stderr:
            print(result.stderr, file=sys.stderr)
        sys.exit(result.returncode)
    return result


# --------------------------------------------------------------------------
# Image / kernel builds
# --------------------------------------------------------------------------

def build_images():
    print("=" * 60)
    print("Building server + client images with `container build`")
    print("=" * 60)
    run([
        CONTAINER, "build",
        "-t", SERVER_IMAGE,
        "-f", str(SERVER_DOCKERFILE),
        str(PROJECT_ROOT),
    ])
    run([
        CONTAINER, "build",
        "-t", CLIENT_IMAGE,
        "-f", str(CLIENT_DOCKERFILE),
        str(CLIENT_CONTEXT),
    ])
    print("✓ images built")
    return 0


def build_kernel(force=False):
    print("=" * 60)
    print("Building NFS-enabled kernel for the client container")
    print("=" * 60)
    if KERNEL_IMAGE.exists() and not force:
        print(f"✓ kernel already present: {KERNEL_IMAGE} (use --force to rebuild)")
        return 0
    KERNEL_IMAGE_OUT.mkdir(parents=True, exist_ok=True)
    run([
        CONTAINER, "build",
        "-c", "6", "-m", "8G",
        "--target", "artifact",
        "-o", f"type=local,dest={KERNEL_IMAGE_OUT}",
        "-t", "arcticwolf-kernel:build",
        "-f", str(KERNEL_DOCKERFILE),
        str(KERNEL_DIR),
    ])
    if not KERNEL_IMAGE.exists():
        print(f"✗ kernel build did not produce {KERNEL_IMAGE}", file=sys.stderr)
        return 1
    print(f"✓ kernel built: {KERNEL_IMAGE}")
    return 0


# --------------------------------------------------------------------------
# Container lifecycle helpers
# --------------------------------------------------------------------------

def container_state(name):
    """Return the container's state string, or None if it does not exist."""
    result = run([CONTAINER, "inspect", name], check=False, capture=True)
    if result.returncode != 0:
        return None
    try:
        data = json.loads(result.stdout)
        return data[0]["status"]["state"]
    except (json.JSONDecodeError, KeyError, IndexError):
        return None


def container_ip(name):
    """Return the container's IPv4 address (without the CIDR suffix)."""
    result = run([CONTAINER, "inspect", name], check=False, capture=True)
    if result.returncode != 0:
        return None
    try:
        data = json.loads(result.stdout)
        addr = data[0]["status"]["networks"][0]["ipv4Address"]
        return addr.split("/")[0]
    except (json.JSONDecodeError, KeyError, IndexError):
        return None


def remove_container(name):
    run([CONTAINER, "rm", "-f", name], check=False, capture=True)


# --------------------------------------------------------------------------
# Server
# --------------------------------------------------------------------------

def start_server():
    print("=" * 60)
    print("Starting Arctic Wolf server container")
    print("=" * 60)

    # Clean any stale instance first so we always boot a fresh server.
    remove_container(SERVER_CONTAINER)

    run([
        CONTAINER, "run", "-d",
        "--name", SERVER_CONTAINER,
        SERVER_IMAGE,
    ])

    ip = wait_for_server_ready()
    if not ip:
        print("✗ server failed to become ready", file=sys.stderr)
        print("--- server logs ---", file=sys.stderr)
        run([CONTAINER, "logs", SERVER_CONTAINER], check=False)
        return 1
    print(f"✓ server ready at {ip}:{NFS_PORT}")
    return 0


def wait_for_server_ready(timeout=60):
    """Wait until the server has bound NFS and report its IP.

    Readiness is detected from the startup banner ("NFS v3 (TCP) on port
    2049") which is printed only after all three RPC listeners are bound.
    """
    print(f"Waiting for server to bind NFS (timeout {timeout}s)...")
    marker = f"NFS v3 (TCP) on port {NFS_PORT}"
    for i in range(timeout):
        state = container_state(SERVER_CONTAINER)
        if state not in ("running", "stopped", None):
            pass
        logs = run([CONTAINER, "logs", SERVER_CONTAINER], check=False, capture=True)
        if marker in (logs.stdout or "") or marker in (logs.stderr or ""):
            ip = container_ip(SERVER_CONTAINER)
            if ip:
                print(f"✓ server bound NFS after {i + 1}s")
                return ip
        if state == "stopped":
            print("✗ server container exited prematurely", file=sys.stderr)
            return None
        time.sleep(1)
    return None


# --------------------------------------------------------------------------
# Client / test
# --------------------------------------------------------------------------

def run_test(testcase="read,write"):
    print("=" * 60)
    print("Running nfstest_posix in client container")
    print("=" * 60)

    if not KERNEL_IMAGE.exists():
        print(f"✗ kernel image missing: {KERNEL_IMAGE}", file=sys.stderr)
        print("  Run: nfstest.py build-kernel", file=sys.stderr)
        return 1

    ip = container_ip(SERVER_CONTAINER)
    if not ip:
        print("✗ could not determine server IP (is it running?)", file=sys.stderr)
        return 1
    print(f"Server IP: {ip}")

    remove_container(CLIENT_CONTAINER)

    # nfstest_posix handles mount/unmount itself; it just needs the server
    # address, the export, and a mount point. nolock avoids NLM (we mount
    # nolock); noresvport avoids requiring a privileged source port.
    nfstest_cmd = (
        "nfstest_posix "
        f"--server {ip} "
        f"--export {NFS_EXPORT} "
        f"--mtpoint {CLIENT_MOUNT_POINT} "
        "--nfsversion 3 "
        f"--runtest={testcase} "
        # nfstest already injects vers=3,proto=tcp,sec=sys from --nfsversion;
        # only add the extras (mount.nfs rejects a duplicate vers= option).
        "--mtopts nolock,noresvport"
    )

    result = run([
        CONTAINER, "run", "--rm",
        "--name", CLIENT_CONTAINER,
        "--cap-add", "ALL",
        "--kernel", str(KERNEL_IMAGE),
        CLIENT_IMAGE,
        nfstest_cmd,
    ], check=False, stream=True)

    print()
    if result.returncode == 0:
        print("NFS integration test PASSED ✓")
    else:
        print(f"NFS integration test FAILED ✗ (exit {result.returncode})")
    return result.returncode


# --------------------------------------------------------------------------
# Teardown
# --------------------------------------------------------------------------

def stop():
    print("=" * 60)
    print("Stopping test containers")
    print("=" * 60)
    remove_container(CLIENT_CONTAINER)
    remove_container(SERVER_CONTAINER)
    print("✓ containers removed")
    return 0


# --------------------------------------------------------------------------
# CLI
# --------------------------------------------------------------------------

def main():
    parser = argparse.ArgumentParser(description=__doc__,
                                     formatter_class=argparse.RawDescriptionHelpFormatter)
    sub = parser.add_subparsers(dest="command")

    sub.add_parser("build-images", help="Build server and client images")

    pk = sub.add_parser("build-kernel", help="Build NFS-enabled client kernel")
    pk.add_argument("--force", action="store_true", help="Rebuild even if present")

    sub.add_parser("start-server", help="Start the server container")
    sub.add_parser("stop", help="Stop and remove test containers")

    pr = sub.add_parser("run-test", help="Run nfstest_posix against the server")
    pr.add_argument("--testcase", default="read,write")

    pt = sub.add_parser("test", help="start-server + run-test + stop")
    pt.add_argument("--testcase", default="read,write")

    args = parser.parse_args()
    if not args.command:
        parser.print_help()
        return 1

    if args.command == "build-images":
        return build_images()
    if args.command == "build-kernel":
        return build_kernel(force=args.force)
    if args.command == "start-server":
        return start_server()
    if args.command == "stop":
        return stop()
    if args.command == "run-test":
        return run_test(testcase=args.testcase)
    if args.command == "test":
        rc = start_server()
        if rc != 0:
            return rc
        try:
            return run_test(testcase=args.testcase)
        finally:
            stop()
    print(f"Unknown command: {args.command}", file=sys.stderr)
    return 1


if __name__ == "__main__":
    sys.exit(main())
