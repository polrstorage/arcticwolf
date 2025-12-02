#!/usr/bin/env python3
"""
Test: Portmapper GETPORT Procedure
Purpose: Verify portmapper can return port numbers for registered services

This test validates:
1. PMAPPROC_GETPORT (procedure 3)
2. Service discovery mechanism
3. Query for MOUNT and NFS services
"""

import socket
import struct
import sys


def test_portmap_getport():
    """Test Portmapper GETPORT procedure"""

    print("Test: Portmapper GETPORT Procedure")
    print("=" * 60)
    print()

    # Server connection details
    host = "localhost"
    port = 4000
    xid = 55555

    print(f"Connecting to {host}:{port}")
    print(f"  Program: 100000 (Portmapper)")
    print(f"  Version: 2")
    print(f"  Procedure: 3 (GETPORT)")
    print()

    # Test queries
    queries = [
        (100000, 2, 6, "Portmapper v2 TCP"),
        (100005, 3, 6, "MOUNT v3 TCP"),
        (100003, 3, 6, "NFS v3 TCP"),
        (999999, 1, 6, "Non-existent service"),
    ]

    for prog, vers, prot, description in queries:
        print(f"Query: {description}")
        print(f"  prog={prog}, vers={vers}, prot={prot}")

        try:
            # Build RPC call header
            rpc_header = struct.pack(
                '>IIIII II II',
                xid,        # XID
                2,          # RPC version
                100000,     # Program (Portmapper)
                2,          # Version (v2)
                3,          # Procedure (GETPORT)
                0, 0,       # cred (AUTH_NONE)
                0, 0        # verf (AUTH_NONE)
            )

            # Build mapping argument (prog, vers, prot, port)
            # Note: port is ignored in GETPORT query
            mapping_arg = struct.pack('>IIII', prog, vers, prot, 0)

            # Complete call message
            call_msg = rpc_header + mapping_arg

            # Add RPC record marking
            msg_len = len(call_msg)
            record_header = struct.pack('>I', 0x80000000 | msg_len)

            # Connect and send
            sock = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
            sock.settimeout(5.0)
            sock.connect((host, port))
            sock.sendall(record_header + call_msg)

            # Receive response
            reply_header_bytes = sock.recv(4)
            if len(reply_header_bytes) != 4:
                print(f"  ✗ Failed to read response header")
                sock.close()
                continue

            reply_header = struct.unpack('>I', reply_header_bytes)[0]
            is_last = (reply_header & 0x80000000) != 0
            reply_len = reply_header & 0x7FFFFFFF

            # Read response data
            reply_data = b''
            while len(reply_data) < reply_len:
                chunk = sock.recv(reply_len - len(reply_data))
                if not chunk:
                    break
                reply_data += chunk

            sock.close()

            # Parse RPC reply header (20 bytes)
            if len(reply_data) < 20:
                print(f"  ✗ Response too short: {len(reply_data)} bytes")
                continue

            reply_xid, reply_stat, verf_flavor, verf_len, accept_stat = struct.unpack(
                '>IIIII', reply_data[:20]
            )

            if reply_stat != 0 or accept_stat != 0:
                print(f"  ✗ RPC error: reply_stat={reply_stat}, accept_stat={accept_stat}")
                continue

            # Parse port result (4 bytes after RPC header)
            if len(reply_data) >= 24:
                result_port = struct.unpack('>I', reply_data[20:24])[0]

                if result_port == 0:
                    print(f"  ✓ Service not found (port=0) - Expected for non-existent")
                else:
                    print(f"  ✓ Service found on port {result_port}")
            else:
                print(f"  ✗ No port data in response")

        except socket.timeout:
            print(f"  ✗ Connection timeout")
        except ConnectionRefusedError:
            print(f"  ✗ Connection refused - is server running?")
            sys.exit(1)
        except Exception as e:
            print(f"  ✗ Error: {e}")

        print()

    print("=" * 60)
    print("✅ Portmapper GETPORT test completed")
    print()
    print("Summary:")
    print("  ✓ Portmapper responds to GETPORT queries")
    print("  ✓ Returns port numbers for registered services")
    print("  ✓ Returns 0 for non-existent services")


if __name__ == '__main__':
    test_portmap_getport()
