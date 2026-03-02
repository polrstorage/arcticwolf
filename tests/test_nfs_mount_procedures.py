#!/usr/bin/env python3
"""
Test: NFS Mount-Critical Procedures
Purpose: Test ACCESS, FSINFO, and FSSTAT procedures required for mounting

This test validates:
1. MOUNT to get root directory handle
2. ACCESS to check file permissions
3. FSINFO to get filesystem capabilities
4. FSSTAT to get filesystem statistics
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


def parse_rpc_reply(reply_data):
    """Parse RPC reply header"""
    if len(reply_data) < 24:
        raise Exception(f"Response too short: {len(reply_data)} bytes")

    reply_xid, msg_type, reply_stat, verf_flavor, verf_len, accept_stat = struct.unpack(
        '>IIIIII', reply_data[:24]
    )

    if reply_stat != 0 or accept_stat != 0:
        raise Exception(f"RPC error: reply_stat={reply_stat}, accept_stat={accept_stat}")

    return 24


def test_mount_procedures():
    """Test ACCESS, FSINFO, and FSSTAT procedures"""

    print("Test: NFS Mount-Critical Procedures")
    print("=" * 60)
    print()

    host = "localhost"
    port = 2049
    mount_port = portmap_getport(host, 100005, 3)

    # Step 1: MOUNT
    print("Step 1: MOUNT /")
    print("-" * 60)
    mount_xid = 500001
    mount_args = pack_string("/")

    reply_data = rpc_call(host, mount_port, mount_xid, 100005, 3, 1, mount_args)
    offset = parse_rpc_reply(reply_data)

    mount_status = struct.unpack('>I', reply_data[offset:offset+4])[0]
    if mount_status != 0:
        print(f"  ✗ MOUNT failed with status {mount_status}")
        sys.exit(1)

    root_fhandle, _ = unpack_opaque_flex(reply_data, offset + 4)
    print(f"  ✓ Got root handle: {len(root_fhandle)} bytes")
    print()

    # Step 2: ACCESS
    print("Step 2: ACCESS (check root directory permissions)")
    print("-" * 60)
    access_xid = 500002

    # ACCESS3args: fhandle3 (object) + uint32 (access bits)
    access_args = b''

    # Add file handle (variable-length opaque)
    access_args += struct.pack('>I', len(root_fhandle)) + root_fhandle
    padding = (4 - (len(root_fhandle) % 4)) % 4
    access_args += b'\x00' * padding

    # Request all access permissions (READ, LOOKUP, MODIFY, EXTEND, DELETE, EXECUTE)
    ACCESS3_READ = 0x0001
    ACCESS3_LOOKUP = 0x0002
    ACCESS3_MODIFY = 0x0004
    ACCESS3_EXTEND = 0x0008
    ACCESS3_DELETE = 0x0010
    ACCESS3_EXECUTE = 0x0020
    requested_access = ACCESS3_READ | ACCESS3_LOOKUP | ACCESS3_MODIFY

    access_args += struct.pack('>I', requested_access)

    reply_data = rpc_call(host, port, access_xid, 100003, 3, 4, access_args)
    offset = parse_rpc_reply(reply_data)

    # Parse ACCESS3res
    nfs_status = struct.unpack('>I', reply_data[offset:offset+4])[0]
    print(f"  NFS status: {nfs_status} (0=NFS3_OK)")

    if nfs_status != 0:
        print(f"  ✗ ACCESS failed with status {nfs_status}")
        sys.exit(1)

    # Skip fattr3 (84 bytes) and get granted access
    offset += 4 + 84
    granted_access = struct.unpack('>I', reply_data[offset:offset+4])[0]

    print(f"  ✓ Requested access: {requested_access:#06x}")
    print(f"  ✓ Granted access:   {granted_access:#06x}")

    if granted_access & ACCESS3_READ:
        print("    - READ permission granted")
    if granted_access & ACCESS3_LOOKUP:
        print("    - LOOKUP permission granted")
    if granted_access & ACCESS3_MODIFY:
        print("    - MODIFY permission granted")
    print()

    # Step 3: FSINFO
    print("Step 3: FSINFO (get filesystem capabilities)")
    print("-" * 60)
    fsinfo_xid = 500003

    # FSINFO3args: fhandle3 (fsroot)
    fsinfo_args = b''
    fsinfo_args += struct.pack('>I', len(root_fhandle)) + root_fhandle
    padding = (4 - (len(root_fhandle) % 4)) % 4
    fsinfo_args += b'\x00' * padding

    reply_data = rpc_call(host, port, fsinfo_xid, 100003, 3, 19, fsinfo_args)
    offset = parse_rpc_reply(reply_data)

    # Parse FSINFO3res
    nfs_status = struct.unpack('>I', reply_data[offset:offset+4])[0]
    print(f"  NFS status: {nfs_status} (0=NFS3_OK)")

    if nfs_status != 0:
        print(f"  ✗ FSINFO failed with status {nfs_status}")
        sys.exit(1)

    # Parse FSINFO3resok
    offset += 4 + 84  # status + fattr3

    rtmax = struct.unpack('>I', reply_data[offset:offset+4])[0]
    offset += 4
    rtpref = struct.unpack('>I', reply_data[offset:offset+4])[0]
    offset += 4
    rtmult = struct.unpack('>I', reply_data[offset:offset+4])[0]
    offset += 4
    wtmax = struct.unpack('>I', reply_data[offset:offset+4])[0]
    offset += 4
    wtpref = struct.unpack('>I', reply_data[offset:offset+4])[0]
    offset += 4
    wtmult = struct.unpack('>I', reply_data[offset:offset+4])[0]
    offset += 4
    dtpref = struct.unpack('>I', reply_data[offset:offset+4])[0]
    offset += 4
    maxfilesize = struct.unpack('>Q', reply_data[offset:offset+8])[0]
    offset += 8
    time_delta_sec = struct.unpack('>I', reply_data[offset:offset+4])[0]
    offset += 4
    time_delta_nsec = struct.unpack('>I', reply_data[offset:offset+4])[0]
    offset += 4
    properties = struct.unpack('>I', reply_data[offset:offset+4])[0]

    print(f"  ✓ Read Transfer:")
    print(f"    Max:  {rtmax} bytes ({rtmax // 1024} KB)")
    print(f"    Pref: {rtpref} bytes ({rtpref // 1024} KB)")
    print(f"    Mult: {rtmult} bytes")
    print(f"  ✓ Write Transfer:")
    print(f"    Max:  {wtmax} bytes ({wtmax // 1024} KB)")
    print(f"    Pref: {wtpref} bytes ({wtpref // 1024} KB)")
    print(f"    Mult: {wtmult} bytes")
    print(f"  ✓ Directory read pref: {dtpref} bytes")
    print(f"  ✓ Max file size: {maxfilesize:#x}")
    print(f"  ✓ Time delta: {time_delta_sec}s + {time_delta_nsec}ns")
    print(f"  ✓ Properties: {properties:#06x}")
    print()

    # Step 4: FSSTAT
    print("Step 4: FSSTAT (get filesystem statistics)")
    print("-" * 60)
    fsstat_xid = 500004

    # FSSTAT3args: fhandle3 (fsroot)
    fsstat_args = b''
    fsstat_args += struct.pack('>I', len(root_fhandle)) + root_fhandle
    padding = (4 - (len(root_fhandle) % 4)) % 4
    fsstat_args += b'\x00' * padding

    reply_data = rpc_call(host, port, fsstat_xid, 100003, 3, 18, fsstat_args)
    offset = parse_rpc_reply(reply_data)

    # Parse FSSTAT3res
    nfs_status = struct.unpack('>I', reply_data[offset:offset+4])[0]
    print(f"  NFS status: {nfs_status} (0=NFS3_OK)")

    if nfs_status != 0:
        print(f"  ✗ FSSTAT failed with status {nfs_status}")
        sys.exit(1)

    # Parse FSSTAT3resok
    offset += 4 + 84  # status + fattr3

    tbytes = struct.unpack('>Q', reply_data[offset:offset+8])[0]
    offset += 8
    fbytes = struct.unpack('>Q', reply_data[offset:offset+8])[0]
    offset += 8
    abytes = struct.unpack('>Q', reply_data[offset:offset+8])[0]
    offset += 8
    tfiles = struct.unpack('>Q', reply_data[offset:offset+8])[0]
    offset += 8
    ffiles = struct.unpack('>Q', reply_data[offset:offset+8])[0]
    offset += 8
    afiles = struct.unpack('>Q', reply_data[offset:offset+8])[0]
    offset += 8
    invarsec = struct.unpack('>I', reply_data[offset:offset+4])[0]

    print(f"  ✓ Total bytes:     {tbytes:,} ({tbytes // (1024**3)} GB)")
    print(f"  ✓ Free bytes:      {fbytes:,} ({fbytes // (1024**3)} GB)")
    print(f"  ✓ Available bytes: {abytes:,} ({abytes // (1024**3)} GB)")
    print(f"  ✓ Total inodes:    {tfiles:,}")
    print(f"  ✓ Free inodes:     {ffiles:,}")
    print(f"  ✓ Avail inodes:    {afiles:,}")
    print(f"  ✓ Invariant time:  {invarsec}s")
    print()

    print("✅ ALL MOUNT-CRITICAL PROCEDURES PASSED")
    print()
    print("Summary:")
    print("  ✓ ACCESS procedure implemented and working")
    print("  ✓ FSINFO procedure implemented and working")
    print("  ✓ FSSTAT procedure implemented and working")
    print()
    print("The server now has all critical procedures for mount operation!")
    print("These procedures are required by NFS clients during mount:")
    print("  - ACCESS: checks file access permissions")
    print("  - FSINFO: queries filesystem capabilities (max sizes, etc.)")
    print("  - FSSTAT: queries filesystem statistics (free space, etc.)")


if __name__ == '__main__':
    test_mount_procedures()
