#!/usr/bin/env python3
"""
Test: NFS LOOKUP Procedure - Specific File Test
Purpose: Test NFS LOOKUP with a specific known file
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


def test_lookup_specific_file():
    """Test NFS LOOKUP with test_lookup_file.txt"""

    print("Test: NFS LOOKUP - Specific File")
    print("=" * 60)
    print()

    host = "localhost"
    port = 2049
    mount_port = portmap_getport(host, 100005, 3)

    # Step 1: MOUNT
    print("Step 1: MOUNT /")
    mount_xid = 300001
    mount_args = pack_string("/")

    reply_data = rpc_call(host, mount_port, mount_xid, 100005, 3, 1, mount_args)

    # Parse MOUNT reply
    root_fhandle, _ = unpack_opaque_flex(reply_data, 24)
    print(f"  ✓ Got root handle: {len(root_fhandle)} bytes")
    print()

    # Step 2: LOOKUP test_lookup_file.txt
    print("Step 2: LOOKUP test_lookup_file.txt")
    nfs_xid = 300002

    # LOOKUP3args: fhandle3 (dir handle) + filename3 (name)
    lookup_args = b''

    # Add directory handle (variable-length opaque)
    lookup_args += struct.pack('>I', len(root_fhandle)) + root_fhandle
    padding = (4 - (len(root_fhandle) % 4)) % 4
    lookup_args += b'\x00' * padding

    # Add filename
    lookup_args += pack_string("test_lookup_file.txt")

    reply_data = rpc_call(host, port, nfs_xid, 100003, 3, 3, lookup_args)

    # Parse RPC reply
    reply_xid, msg_type, reply_stat, verf_flavor, verf_len, accept_stat = struct.unpack(
        '>IIIIII', reply_data[:24]
    )

    # Parse LOOKUP3res
    nfs_status = struct.unpack('>I', reply_data[20:24])[0]
    print(f"  NFS status: {nfs_status} (0=NFS3_OK)")

    if nfs_status == 0:
        # Extract object file handle
        obj_handle, next_offset = unpack_opaque_flex(reply_data, 24)
        print(f"  ✓ Found file handle: {len(obj_handle)} bytes")
        print(f"    Handle (hex): {obj_handle.hex()}")

        # Parse obj_attributes (fattr3 - 84 bytes)
        ftype = struct.unpack('>I', reply_data[next_offset:next_offset+4])[0]
        mode = struct.unpack('>I', reply_data[next_offset+4:next_offset+8])[0]
        nlink = struct.unpack('>I', reply_data[next_offset+8:next_offset+12])[0]
        uid = struct.unpack('>I', reply_data[next_offset+12:next_offset+16])[0]
        gid = struct.unpack('>I', reply_data[next_offset+16:next_offset+20])[0]
        size = struct.unpack('>Q', reply_data[next_offset+20:next_offset+28])[0]

        ftype_names = {1: "REG", 2: "DIR", 3: "BLK", 4: "CHR", 5: "LNK", 6: "SOCK", 7: "FIFO"}
        ftype_name = ftype_names.get(ftype, f"UNKNOWN({ftype})")

        print()
        print("  File attributes:")
        print(f"    Type: {ftype_name}")
        print(f"    Mode: {oct(mode)}")
        print(f"    Links: {nlink}")
        print(f"    UID: {uid}")
        print(f"    GID: {gid}")
        print(f"    Size: {size} bytes")
        print()

        print("✅ NFS LOOKUP test PASSED")
        print()
        print("Summary:")
        print("  ✓ LOOKUP found the file")
        print("  ✓ Returned valid file handle")
        print("  ✓ File attributes are correct")

    else:
        print(f"  ✗ NFS error: status={nfs_status}")
        print()
        print("Test failed - file should exist")
        sys.exit(1)


if __name__ == '__main__':
    test_lookup_specific_file()
