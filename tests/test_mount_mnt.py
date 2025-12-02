#!/usr/bin/env python3
"""
Test: MOUNT MNT Procedure
Purpose: Verify MOUNT protocol MNT procedure (mount directory and get file handle)

This test validates:
1. MOUNT MNT procedure (procedure 1)
2. Directory path argument encoding (XDR string)
3. File handle response parsing
4. mountres3 union parsing
"""

import socket
import struct
import sys


def pack_xdr_string(s):
    """Pack a string in XDR format"""
    # XDR string format:
    # - Length (4 bytes, big-endian)
    # - Data (variable length)
    # - Padding to 4-byte boundary with zeros
    encoded = s.encode('utf-8')
    length = len(encoded)
    padding = (4 - (length % 4)) % 4
    return struct.pack('>I', length) + encoded + (b'\x00' * padding)


def unpack_xdr_opaque_flex(data, offset):
    """Unpack XDR variable-length opaque data (length + bytes)"""
    length = struct.unpack('>I', data[offset:offset+4])[0]
    offset += 4
    value = data[offset:offset+length]
    offset += length
    # Skip padding to 4-byte boundary
    padding = (4 - (length % 4)) % 4
    offset += padding
    return value, offset


def test_mount_mnt():
    """Test MOUNT MNT procedure (mount a directory path)"""

    print("Test: MOUNT MNT Procedure")
    print("=" * 60)
    print()

    # Server connection details
    host = "localhost"
    port = 4000
    xid = 99999  # Transaction ID
    mount_path = "/export/test"

    print(f"Connecting to {host}:{port}")
    print(f"  Program: 100005 (MOUNT)")
    print(f"  Version: 3 (MOUNTv3)")
    print(f"  Procedure: 1 (MNT)")
    print(f"  XID: {xid}")
    print(f"  Path: '{mount_path}'")
    print()

    # Build RPC call header
    rpc_header = struct.pack(
        '>IIIII II II',
        xid,        # XID
        2,          # RPC version
        100005,     # Program (MOUNT)
        3,          # Version (MOUNTv3)
        1,          # Procedure (MNT)
        0, 0,       # cred (AUTH_NONE, length 0)
        0, 0        # verf (AUTH_NONE, length 0)
    )

    # Pack directory path as XDR string
    path_data = pack_xdr_string(mount_path)

    # Complete call message
    call_msg = rpc_header + path_data

    # Add RPC record marking header
    msg_len = len(call_msg)
    record_header = struct.pack('>I', 0x80000000 | msg_len)

    print(f"Request:")
    print(f"  RPC header: {len(rpc_header)} bytes")
    print(f"  Path data: {len(path_data)} bytes")
    print(f"  Total message: {msg_len} bytes")
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

        print(f"Response ({len(reply_data)} bytes, hex): {reply_data.hex()}")
        print()

        # Parse RPC reply header (20 bytes)
        if len(reply_data) < 20:
            print(f"✗ Response too short: {len(reply_data)} bytes (expected at least 20)")
            sys.exit(1)

        reply_xid, reply_stat, verf_flavor, verf_len, accept_stat = struct.unpack(
            '>IIIII', reply_data[:20]
        )

        print("RPC Reply Header:")
        print(f"  XID: {reply_xid} (expected {xid})")
        print(f"  Reply stat: {reply_stat} (0=MSG_ACCEPTED)")
        print(f"  Verf flavor: {verf_flavor} (0=AUTH_NONE)")
        print(f"  Verf length: {verf_len}")
        print(f"  Accept stat: {accept_stat} (0=SUCCESS)")
        print()

        # Validate RPC header
        if reply_xid != xid:
            print(f"✗ XID mismatch: expected {xid}, got {reply_xid}")
            sys.exit(1)
        if reply_stat != 0:
            print(f"✗ Reply stat should be 0 (MSG_ACCEPTED), got {reply_stat}")
            sys.exit(1)
        if accept_stat != 0:
            print(f"✗ Accept stat should be 0 (SUCCESS), got {accept_stat}")
            sys.exit(1)

        # Parse MOUNT response (after RPC header)
        # mountres3 union format:
        #   status (4 bytes) - discriminant
        #   if status == MNT3_OK (0):
        #     fhandle3 (variable length opaque)
        #     auth_flavors (variable length array of int)
        offset = 20

        if offset >= len(reply_data):
            print("✗ No MOUNT data after RPC header")
            sys.exit(1)

        # Parse mountstat3 (discriminant)
        mount_status = struct.unpack('>I', reply_data[offset:offset+4])[0]
        offset += 4

        print("MOUNT Response:")
        print(f"  Status: {mount_status} (0=MNT3_OK)")

        if mount_status == 0:  # MNT3_OK
            # Parse file handle (opaque<>)
            fhandle, offset = unpack_xdr_opaque_flex(reply_data, offset)
            print(f"  File handle: {len(fhandle)} bytes")
            print(f"  File handle (hex): {fhandle.hex()}")

            # Parse auth_flavors array (int<>)
            if offset < len(reply_data):
                num_flavors = struct.unpack('>I', reply_data[offset:offset+4])[0]
                offset += 4
                print(f"  Auth flavors: {num_flavors} entries")

                flavors = []
                for i in range(num_flavors):
                    if offset + 4 <= len(reply_data):
                        flavor = struct.unpack('>I', reply_data[offset:offset+4])[0]
                        flavors.append(flavor)
                        offset += 4

                print(f"  Flavors: {flavors}")

            print()
            print("✅ MOUNT MNT procedure succeeded!")
            print()
            print("Summary:")
            print(f"  ✓ Mounted path: '{mount_path}'")
            print(f"  ✓ Received file handle: {len(fhandle)} bytes")
            print(f"  ✓ File handle can be used for NFS operations")

        else:
            # Error status
            error_names = {
                1: "MNT3ERR_PERM",
                2: "MNT3ERR_NOENT",
                5: "MNT3ERR_IO",
                13: "MNT3ERR_ACCESS",
                20: "MNT3ERR_NOTDIR",
                22: "MNT3ERR_INVAL",
                63: "MNT3ERR_NAMETOOLONG",
                10004: "MNT3ERR_NOTSUPP",
                10006: "MNT3ERR_SERVERFAULT"
            }
            error_name = error_names.get(mount_status, f"UNKNOWN({mount_status})")
            print(f"✗ MOUNT failed with error: {error_name}")
            sys.exit(1)

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
    test_mount_mnt()
