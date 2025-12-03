#!/usr/bin/env python3
"""
Test: NFS NULL Procedure
Purpose: Verify NFS service responds to NULL procedure calls

This test validates:
1. NFS NULL procedure (procedure 0)
2. Basic NFS protocol connectivity
3. NFS version 3 support
"""

import socket
import struct
import sys


def test_nfs_null():
    """Test NFS NULL procedure"""

    print("Test: NFS NULL Procedure")
    print("=" * 60)
    print()

    # Server connection details
    host = "localhost"
    port = 4000
    xid = 99999

    print(f"Connecting to {host}:{port}")
    print(f"  Program: 100003 (NFS)")
    print(f"  Version: 3 (NFSv3)")
    print(f"  Procedure: 0 (NULL)")
    print()

    try:
        # Build RPC call header for NFS NULL (same format as other RPC tests)
        message = b''
        message += struct.pack('>I', xid)      # XID
        message += struct.pack('>I', 2)        # RPC version
        message += struct.pack('>I', 100003)   # Program (NFS)
        message += struct.pack('>I', 3)        # Version (NFSv3)
        message += struct.pack('>I', 0)        # Procedure (NULL)
        # cred (AUTH_NONE)
        message += struct.pack('>I', 0)        # flavor = AUTH_NONE
        message += struct.pack('>I', 0)        # length = 0
        # verf (AUTH_NONE)
        message += struct.pack('>I', 0)        # flavor = AUTH_NONE
        message += struct.pack('>I', 0)        # length = 0

        # NULL procedure has no arguments
        call_msg = message

        # Add RPC record marking
        msg_len = len(call_msg)
        record_header = struct.pack('>I', 0x80000000 | msg_len)

        # Connect and send
        print("Sending NFS NULL request...")
        sock = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
        sock.settimeout(5.0)
        sock.connect((host, port))
        sock.sendall(record_header + call_msg)

        # Receive response
        print("Waiting for response...")
        reply_header_bytes = sock.recv(4)
        if len(reply_header_bytes) != 4:
            print("  ✗ Failed to read response header")
            sock.close()
            sys.exit(1)

        reply_header = struct.unpack('>I', reply_header_bytes)[0]
        is_last = (reply_header & 0x80000000) != 0
        reply_len = reply_header & 0x7FFFFFFF

        print(f"  Response header: is_last={is_last}, length={reply_len}")

        # Read response data
        reply_data = b''
        while len(reply_data) < reply_len:
            chunk = sock.recv(reply_len - len(reply_data))
            if not chunk:
                break
            reply_data += chunk

        sock.close()

        print(f"  Received {len(reply_data)} bytes")

        # Parse RPC reply header
        # Format: xid(4) + reply_stat(4) + verf_flavor(4) + verf_len(4) + accept_stat(4)
        if len(reply_data) < 20:
            print(f"  ✗ Response too short: {len(reply_data)} bytes")
            sys.exit(1)

        reply_xid, reply_stat, verf_flavor, verf_len, accept_stat = struct.unpack(
            '>IIIII', reply_data[:20]
        )

        print(f"  Reply XID: {reply_xid}")
        print(f"  Reply stat: {reply_stat} (0=MSG_ACCEPTED)")
        print(f"  Verf flavor: {verf_flavor}")
        print(f"  Verf length: {verf_len}")
        print(f"  Accept stat: {accept_stat} (0=SUCCESS)")
        print()

        # Verify response
        if reply_xid != xid:
            print(f"  ✗ XID mismatch: expected {xid}, got {reply_xid}")
            sys.exit(1)

        if reply_stat != 0:  # MSG_ACCEPTED = 0
            print(f"  ✗ Wrong reply_stat: expected 0 (MSG_ACCEPTED), got {reply_stat}")
            sys.exit(1)

        if accept_stat != 0:  # SUCCESS = 0
            print(f"  ✗ RPC error: accept_stat={accept_stat}")
            sys.exit(1)

        print("✅ NFS NULL test PASSED")
        print()
        print("Summary:")
        print("  ✓ NFS service is running")
        print("  ✓ NFS NULL procedure works correctly")
        print("  ✓ NFSv3 protocol layer functional")

    except socket.timeout:
        print("  ✗ Connection timeout")
        sys.exit(1)
    except ConnectionRefusedError:
        print("  ✗ Connection refused - is server running?")
        sys.exit(1)
    except Exception as e:
        print(f"  ✗ Error: {e}")
        import traceback
        traceback.print_exc()
        sys.exit(1)


if __name__ == '__main__':
    test_nfs_null()
