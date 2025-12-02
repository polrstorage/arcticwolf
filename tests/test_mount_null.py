#!/usr/bin/env python3
"""
Test: MOUNT NULL Procedure
Purpose: Verify MOUNT protocol NULL procedure (ping/connectivity test)

This test validates:
1. MOUNT protocol routing (program 100005)
2. MOUNT NULL procedure (procedure 0)
3. RPC response format for MOUNT protocol
"""

import socket
import struct
import sys


def test_mount_null():
    """Test MOUNT NULL procedure (program 100005, procedure 0)"""

    print("Test: MOUNT NULL Procedure")
    print("=" * 60)
    print()

    # Server connection details
    host = "localhost"
    port = 4000
    xid = 67890  # Transaction ID

    print(f"Connecting to {host}:{port}")
    print(f"  Program: 100005 (MOUNT)")
    print(f"  Version: 3 (MOUNTv3)")
    print(f"  Procedure: 0 (NULL)")
    print(f"  XID: {xid}")
    print()

    # Build RPC MOUNT NULL call
    # Structure (36 bytes total for AUTH_NONE):
    #   xid          (4 bytes)
    #   rpcvers      (4 bytes) = 2
    #   prog         (4 bytes) = 100005 (MOUNT)
    #   vers         (4 bytes) = 3
    #   proc         (4 bytes) = 0 (NULL)
    #   cred.flavor  (4 bytes) = 0 (AUTH_NONE)
    #   cred.length  (4 bytes) = 0
    #   verf.flavor  (4 bytes) = 0 (AUTH_NONE)
    #   verf.length  (4 bytes) = 0
    call_msg = struct.pack(
        '>IIIII II II',
        xid,        # XID
        2,          # RPC version
        100005,     # Program (MOUNT)
        3,          # Version (MOUNTv3)
        0,          # Procedure (NULL)
        0, 0,       # cred (AUTH_NONE, length 0)
        0, 0        # verf (AUTH_NONE, length 0)
    )

    # Add RPC record marking header
    # Format: [last_fragment:1bit][length:31bits]
    # For last fragment of 36 bytes: 0x80000024
    msg_len = len(call_msg)
    record_header = struct.pack('>I', 0x80000000 | msg_len)

    print(f"Request:")
    print(f"  Message size: {msg_len} bytes")
    print(f"  Record marking: 0x{struct.unpack('>I', record_header)[0]:08x}")
    print(f"  Message (hex): {call_msg.hex()}")
    print()

    # Connect and send
    try:
        sock = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
        sock.settimeout(5.0)
        sock.connect((host, port))

        sock.sendall(record_header + call_msg)
        print("✓ Request sent")
        print()

        # Receive response
        print("Waiting for response...")

        # Read record marking header (4 bytes)
        reply_header_bytes = sock.recv(4)
        if len(reply_header_bytes) != 4:
            print(f"✗ Failed to read response header (got {len(reply_header_bytes)} bytes)")
            sys.exit(1)

        reply_header = struct.unpack('>I', reply_header_bytes)[0]
        is_last = (reply_header & 0x80000000) != 0
        reply_len = reply_header & 0x7FFFFFFF

        print(f"Response fragment: last={is_last}, length={reply_len}")

        # Read response data
        reply_data = b''
        while len(reply_data) < reply_len:
            chunk = sock.recv(reply_len - len(reply_data))
            if not chunk:
                break
            reply_data += chunk

        sock.close()

        if len(reply_data) != reply_len:
            print(f"✗ Incomplete response: expected {reply_len}, got {len(reply_data)} bytes")
            sys.exit(1)

        print(f"Response (hex): {reply_data.hex()}")
        print()

        # Parse RPC reply
        # Expected structure (20 bytes for successful NULL):
        #   xid          (4 bytes)
        #   reply_stat   (4 bytes) = 0 (MSG_ACCEPTED)
        #   verf.flavor  (4 bytes) = 0 (AUTH_NONE)
        #   verf.length  (4 bytes) = 0
        #   accept_stat  (4 bytes) = 0 (SUCCESS)
        if len(reply_data) < 20:
            print(f"✗ Response too short: {len(reply_data)} bytes (expected at least 20)")
            sys.exit(1)

        reply_xid, reply_stat, verf_flavor, verf_len, accept_stat = struct.unpack(
            '>IIIII', reply_data[:20]
        )

        print("Parsed response:")
        print(f"  XID: {reply_xid} (expected {xid})")
        print(f"  Reply stat: {reply_stat} (0=MSG_ACCEPTED)")
        print(f"  Verf flavor: {verf_flavor} (0=AUTH_NONE)")
        print(f"  Verf length: {verf_len}")
        print(f"  Accept stat: {accept_stat} (0=SUCCESS)")
        print()

        # Validate response
        errors = []
        if reply_xid != xid:
            errors.append(f"XID mismatch: expected {xid}, got {reply_xid}")
        if reply_stat != 0:
            errors.append(f"Reply stat should be 0 (MSG_ACCEPTED), got {reply_stat}")
        if accept_stat != 0:
            errors.append(f"Accept stat should be 0 (SUCCESS), got {accept_stat}")

        if errors:
            print("✗ Validation failed:")
            for error in errors:
                print(f"  - {error}")
            sys.exit(1)

        print("✅ MOUNT NULL procedure succeeded!")
        print()
        print("Summary:")
        print("  ✓ MOUNT protocol routing works (program 100005)")
        print("  ✓ MOUNT NULL procedure works (procedure 0)")
        print("  ✓ Response format is correct")

    except socket.timeout:
        print("✗ Connection timeout")
        sys.exit(1)
    except ConnectionRefusedError:
        print(f"✗ Connection refused - is the server running on {host}:{port}?")
        sys.exit(1)
    except Exception as e:
        print(f"✗ Error: {e}")
        import traceback
        traceback.print_exc()
        sys.exit(1)


if __name__ == '__main__':
    test_mount_null()
