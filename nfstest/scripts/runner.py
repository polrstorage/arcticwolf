#!/usr/bin/env python3
"""Script to run nfstest against the NFS server.

This runs inside the Alpine VM.
"""

import argparse
import os
import socket
import subprocess
import sys
import time

# Configuration
NFS_SERVER = "10.0.2.2"  # Host via QEMU user networking
NFS_PORT = 4000
MOUNT_POINT = "/mnt/nfstest"
NFSTEST_DIR = "/opt/nfstest"


def check_port(host, port, timeout=1):
    """Check if a port is reachable."""
    try:
        sock = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
        sock.settimeout(timeout)
        result = sock.connect_ex((host, port))
        sock.close()
        return result == 0
    except Exception:
        return False


def wait_for_server(host, port, max_attempts=30):
    """Wait for NFS server to be reachable."""
    print("Waiting for NFS server...", flush=True)
    for i in range(1, max_attempts + 1):
        if check_port(host, port):
            print("NFS server is reachable", flush=True)
            return True
        if i == max_attempts:
            print(f"ERROR: NFS server not reachable after {max_attempts} seconds", file=sys.stderr, flush=True)
            return False
        time.sleep(1)
    return False


def run_nfstest(testcase="open,read,write"):
    """Run nfstest_posix."""
    print("Running nfstest_posix...", flush=True)
    print(flush=True)

    # Set up environment for nfstest
    env = os.environ.copy()
    env['PYTHONPATH'] = NFSTEST_DIR
    env['PATH'] = f"{NFSTEST_DIR}/test:{env.get('PATH', '')}"

    # POSIX compliance test
    # nfstest_posix will handle mount/unmount
    nfstest_posix = os.path.join(NFSTEST_DIR, "test", "nfstest_posix")
    cmd = [
        nfstest_posix,
        "--server", NFS_SERVER,
        "--export", "/",
        "--mtpoint", MOUNT_POINT,
        "--nfsversion", "3",
        "--port", str(NFS_PORT),
        f"--runtest={testcase}",
        "--mtopts", f"vers=3,proto=tcp,port={NFS_PORT},mountport={NFS_PORT},nolock,noresvport"
    ]

    # Flush before subprocess to ensure ordering
    sys.stdout.flush()
    sys.stderr.flush()

    result = subprocess.run(cmd, env=env)
    return result.returncode


def main():
    """Main entry point."""
    parser = argparse.ArgumentParser(description="Run NFS tests against the server")
    parser.add_argument("--testcase", default="open,read,write", help="Test cases to run (default: open,read,write)")
    args = parser.parse_args()

    print(f"NFS Server: {NFS_SERVER}:{NFS_PORT}", flush=True)
    print(flush=True)

    # Wait for NFS server to be reachable
    if not wait_for_server(NFS_SERVER, NFS_PORT):
        return 1

    print(flush=True)

    # Run nfstest_posix
    result = run_nfstest(testcase=args.testcase)

    # Flush again after subprocess completes
    sys.stdout.flush()
    sys.stderr.flush()

    print(flush=True)
    print("Test Complete", flush=True)

    return result


if __name__ == "__main__":
    sys.exit(main())
