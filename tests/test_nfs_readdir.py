#!/usr/bin/env python3
"""
Test: NFS READDIR Procedure
Purpose: Test directory listing functionality
"""

import socket
import struct
import sys
import os

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
    return reply_data


def parse_rpc_reply(reply_data):
    """Parse RPC reply header"""
    if len(reply_data) < 24:
        raise Exception(f"Response too short: {len(reply_data)} bytes")

    reply_xid, msg_type, reply_stat, verf_flavor, verf_len, accept_stat = struct.unpack(
        '>IIIIII', reply_data[:24]
    )

    print(f"  Reply XID: {reply_xid}")
    print(f"  Reply stat: {reply_stat} (0=MSG_ACCEPTED)")
    print(f"  Verf flavor: {verf_flavor}")
    print(f"  Verf length: {verf_len}")
    print(f"  Accept stat: {accept_stat} (0=SUCCESS)")

    if reply_stat != 0 or accept_stat != 0:
        raise Exception(f"RPC error: reply_stat={reply_stat}, accept_stat={accept_stat}")

    return 24


def test_nfs_readdir():
    """Test NFS READDIR procedure"""

    print("Test: NFS READDIR Procedure")
    print("=" * 70)
    print()

    host = "localhost"
    port = 2049
    mount_port = portmap_getport(host, 100005, 3)
    print(f"  Discovered MOUNT port: {mount_port}")

    # Step 1: MOUNT to get root handle
    print("Step 1: MOUNT /")
    print("-" * 70)
    mount_xid = 800001
    mount_args = pack_string("/")

    reply_data = rpc_call(host, mount_port, mount_xid, 100005, 3, 1, mount_args)

    offset = parse_rpc_reply(reply_data)

    mount_status = struct.unpack('>I', reply_data[offset:offset+4])[0]
    if mount_status != 0:
        print(f"  ✗ MOUNT failed with status {mount_status}")
        sys.exit(1)

    root_fhandle, _ = unpack_opaque_flex(reply_data, offset + 4)
    print(f"  ✓ Got root handle: {len(root_fhandle)} bytes")
    print(f"    Handle (hex): {root_fhandle.hex()}")
    print()

    # Step 2: READDIR
    print("Step 2: READDIR (list root directory)")
    print("-" * 70)
    readdir_xid = 800002

    # READDIR3args: fhandle3 (dir) + cookie3 + cookieverf3 + count
    readdir_args = b''

    # fhandle3 (dir)
    readdir_args += struct.pack('>I', len(root_fhandle)) + root_fhandle
    padding = (4 - (len(root_fhandle) % 4)) % 4
    readdir_args += b'\x00' * padding

    # cookie3 (start from beginning)
    readdir_args += struct.pack('>Q', 0)

    # cookieverf3 (8 bytes)
    readdir_args += b'\x00' * 8

    # count (max bytes to return)
    readdir_args += struct.pack('>I', 8192)

    print(f"  READDIR args length: {len(readdir_args)} bytes")
    print(f"  Cookie: 0 (from beginning)")
    print(f"  Count: 8192 bytes")
    print()

    reply_data = rpc_call(host, port, readdir_xid, 100003, 3, 16, readdir_args)

    offset = parse_rpc_reply(reply_data)

    # Parse NFS status
    nfs_status = struct.unpack('>I', reply_data[offset:offset+4])[0]
    print(f"  NFS status: {nfs_status} (0=NFS3_OK)")
    offset += 4

    if nfs_status != 0:
        print(f"  ✗ READDIR failed with status {nfs_status}")
        sys.exit(1)

    # Parse post_op_attr (dir_attributes)
    # For simplicity, just skip fattr3 (84 bytes after the bool)
    attr_follows = struct.unpack('>I', reply_data[offset:offset+4])[0]
    offset += 4

    if attr_follows == 1:
        offset += 84  # Skip fattr3

    # Skip cookieverf (8 bytes)
    offset += 8

    # Parse dirlist3 (entries list + eof)
    print(f"  Parsing directory entries:")

    entries_count = 0
    # Check if there are entries (Option<Box<entry3>>)
    has_entries = struct.unpack('>I', reply_data[offset:offset+4])[0]
    offset += 4

    while has_entries == 1:
        # Parse entry3: fileid + name + cookie + nextentry
        fileid = struct.unpack('>Q', reply_data[offset:offset+8])[0]
        offset += 8

        # Parse filename (string)
        name_len = struct.unpack('>I', reply_data[offset:offset+4])[0]
        offset += 4
        name = reply_data[offset:offset+name_len].decode('utf-8')
        offset += name_len
        # Padding
        name_padding = (4 - (name_len % 4)) % 4
        offset += name_padding

        # Cookie
        cookie = struct.unpack('>Q', reply_data[offset:offset+8])[0]
        offset += 8

        entries_count += 1
        print(f"    [{entries_count}] fileid={fileid}, name='{name}', cookie={cookie}")

        # Check for next entry
        has_entries = struct.unpack('>I', reply_data[offset:offset+4])[0]
        offset += 4

    # EOF flag
    eof = struct.unpack('>I', reply_data[offset:offset+4])[0]
    offset += 4

    print()
    print(f"  ✓ Found {entries_count} entries")
    print(f"  ✓ EOF: {eof == 1}")
    print()

    # Summary
    print("=" * 70)
    print("✅ NFS READDIR test PASSED")
    print()
    print("Summary:")
    print(f"  ✓ READDIR procedure works")
    print(f"  ✓ Directory listing successful")
    print(f"  ✓ Found {entries_count} entries in root directory")
    print(f"  ✓ Response format correct")


if __name__ == '__main__':
    test_nfs_readdir()
