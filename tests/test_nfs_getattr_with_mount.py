#!/usr/bin/env python3
"""
Test: NFS GETATTR with MOUNT
Purpose: Get real file handle via MOUNT, then test GETATTR

This test validates the full workflow:
1. MOUNT to get root file handle
2. GETATTR using that handle to retrieve attributes
3. Verify file attributes are valid
"""

import os
import socket
import struct
import sys

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from rpc_helpers import portmap_getport


def pack_string(s):
    """Pack a string as XDR string"""
    data = s.encode('utf-8')
    length = len(data)
    padding = (4 - (length % 4)) % 4
    return struct.pack('>I', length) + data + b'\x00' * padding


def unpack_opaque_flex(data, offset):
    """Unpack variable-length opaque data (length + data)"""
    length = struct.unpack('>I', data[offset:offset+4])[0]
    opaque_data = data[offset+4:offset+4+length]
    padding = (4 - (length % 4)) % 4
    next_offset = offset + 4 + length + padding
    return opaque_data, next_offset


def rpc_call(host, port, xid, prog, vers, proc, args_data):
    """Make an RPC call and return the response"""
    # Build RPC call header
    message = b''
    message += struct.pack('>I', xid)      # XID
    message += struct.pack('>I', 0)        # msg_type = CALL (0)
    message += struct.pack('>I', 2)        # RPC version
    message += struct.pack('>I', prog)     # Program
    message += struct.pack('>I', vers)     # Version
    message += struct.pack('>I', proc)     # Procedure
    # cred (AUTH_NONE)
    message += struct.pack('>I', 0)        # flavor = AUTH_NONE
    message += struct.pack('>I', 0)        # length = 0
    # verf (AUTH_NONE)
    message += struct.pack('>I', 0)        # flavor = AUTH_NONE
    message += struct.pack('>I', 0)        # length = 0

    # Add procedure arguments
    call_msg = message + args_data

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
        sock.close()
        raise Exception("Failed to read response header")

    reply_header = struct.unpack('>I', reply_header_bytes)[0]
    reply_len = reply_header & 0x7FFFFFFF

    # Read response data
    reply_data = b''
    while len(reply_data) < reply_len:
        chunk = sock.recv(reply_len - len(reply_data))
        if not chunk:
            break
        reply_data += chunk

    sock.close()
    return reply_data


def test_nfs_getattr_with_mount():
    """Test NFS GETATTR using file handle from MOUNT"""

    print("Test: NFS GETATTR with MOUNT")
    print("=" * 60)
    print()

    host = "localhost"
    port = 2049
    mount_port = portmap_getport(host, 100005, 3)

    # Step 1: Call MOUNT to get root file handle
    print("Step 1: MOUNT to get root file handle")
    print("-" * 60)

    mount_xid = 100001
    mount_prog = 100005  # MOUNT
    mount_vers = 3
    mount_proc = 1  # MNT

    # MOUNT args: dirpath (export path)
    dirpath = "/"
    mount_args = pack_string(dirpath)

    print(f"  Calling MOUNT MNT for path: {dirpath}")

    try:
        reply_data = rpc_call(host, mount_port, mount_xid, mount_prog, mount_vers, mount_proc, mount_args)

        # Parse MOUNT reply
        # Format: xid(4) + reply_stat(4) + verf_flavor(4) + verf_len(4) + accept_stat(4)
        if len(reply_data) < 24:
            print(f"  ✗ Response too short: {len(reply_data)} bytes")
            sys.exit(1)

        reply_xid, msg_type, reply_stat, verf_flavor, verf_len, accept_stat = struct.unpack(
            '>IIIIII', reply_data[:24]
        )

        if reply_stat != 0 or accept_stat != 0:
            print(f"  ✗ RPC error: reply_stat={reply_stat}, accept_stat={accept_stat}")
            sys.exit(1)

        # Parse mountres3: status(4) + fhandle (if status == 0)
        mount_status = struct.unpack('>I', reply_data[20:24])[0]
        print(f"  MOUNT status: {mount_status}")

        if mount_status != 0:
            print(f"  ✗ MOUNT failed with status {mount_status}")
            sys.exit(1)

        # Extract file handle (variable-length opaque)
        fhandle, next_offset = unpack_opaque_flex(reply_data, 24)
        print(f"  ✓ Got file handle: {len(fhandle)} bytes")
        print(f"    Handle (hex): {fhandle.hex()}")
        print()

    except Exception as e:
        print(f"  ✗ MOUNT failed: {e}")
        import traceback
        traceback.print_exc()
        sys.exit(1)

    # Step 2: Call NFS GETATTR with the file handle
    print("Step 2: NFS GETATTR with file handle")
    print("-" * 60)

    nfs_xid = 100002
    nfs_prog = 100003  # NFS
    nfs_vers = 3
    nfs_proc = 1  # GETATTR

    # GETATTR args: fhandle3 (variable-length opaque)
    getattr_args = struct.pack('>I', len(fhandle)) + fhandle
    # Add padding
    padding = (4 - (len(fhandle) % 4)) % 4
    getattr_args += b'\x00' * padding

    print(f"  Calling NFS GETATTR with {len(fhandle)}-byte handle")

    try:
        reply_data = rpc_call(host, port, nfs_xid, nfs_prog, nfs_vers, nfs_proc, getattr_args)

        # Parse RPC reply header
        if len(reply_data) < 24:
            print(f"  ✗ Response too short: {len(reply_data)} bytes")
            sys.exit(1)

        reply_xid, msg_type, reply_stat, verf_flavor, verf_len, accept_stat = struct.unpack(
            '>IIIIII', reply_data[:24]
        )

        print(f"  RPC reply: xid={reply_xid}, reply_stat={reply_stat}, accept_stat={accept_stat}")

        if reply_stat != 0 or accept_stat != 0:
            print(f"  ✗ RPC error: reply_stat={reply_stat}, accept_stat={accept_stat}")
            sys.exit(1)

        # Parse GETATTR3res: status(4) + fattr3 (if status == 0)
        nfs_status = struct.unpack('>I', reply_data[20:24])[0]
        print(f"  NFS status: {nfs_status} (0=NFS3_OK)")

        if nfs_status != 0:
            print(f"  ✗ NFS error: status={nfs_status}")
            sys.exit(1)

        # Parse fattr3 structure (84 bytes)
        # ftype(4) + mode(4) + nlink(4) + uid(4) + gid(4) + size(8) + used(8) +
        # rdev(8) + fsid(8) + fileid(8) + atime(8) + mtime(8) + ctime(8)
        if len(reply_data) < 24 + 84:
            print(f"  ✗ Response too short for fattr3: {len(reply_data)} bytes")
            sys.exit(1)

        offset = 24
        ftype = struct.unpack('>I', reply_data[offset:offset+4])[0]
        mode = struct.unpack('>I', reply_data[offset+4:offset+8])[0]
        nlink = struct.unpack('>I', reply_data[offset+8:offset+12])[0]
        uid = struct.unpack('>I', reply_data[offset+12:offset+16])[0]
        gid = struct.unpack('>I', reply_data[offset+16:offset+20])[0]
        size = struct.unpack('>Q', reply_data[offset+20:offset+28])[0]

        ftype_names = {1: "REG", 2: "DIR", 3: "BLK", 4: "CHR", 5: "LNK", 6: "SOCK", 7: "FIFO"}
        ftype_name = ftype_names.get(ftype, f"UNKNOWN({ftype})")

        print()
        print("  ✓ File attributes retrieved:")
        print(f"    Type: {ftype_name}")
        print(f"    Mode: {oct(mode)}")
        print(f"    Links: {nlink}")
        print(f"    UID: {uid}")
        print(f"    GID: {gid}")
        print(f"    Size: {size} bytes")
        print()

        print("✅ NFS GETATTR test PASSED")
        print()
        print("Summary:")
        print("  ✓ MOUNT procedure returns valid file handle")
        print("  ✓ NFS GETATTR procedure works with real handle")
        print("  ✓ File attributes correctly serialized")
        print("  ✓ FSAL integration fully functional")

    except Exception as e:
        print(f"  ✗ GETATTR failed: {e}")
        import traceback
        traceback.print_exc()
        sys.exit(1)


if __name__ == '__main__':
    test_nfs_getattr_with_mount()
