#!/usr/bin/env python3
"""
Test: NFS FSINFO Response Format Debug
Purpose: Debug FSINFO response format to identify parsing issues
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


def hex_dump(data, offset=0, length=None):
    """Print hex dump of data"""
    if length is None:
        length = len(data) - offset

    print(f"  Hex dump (offset {offset}, length {length}):")
    for i in range(0, length, 16):
        hex_str = ' '.join(f'{b:02x}' for b in data[offset+i:offset+i+16])
        ascii_str = ''.join(chr(b) if 32 <= b < 127 else '.' for b in data[offset+i:offset+i+16])
        print(f"    {offset+i:04x}: {hex_str:<48} {ascii_str}")


def test_fsinfo_format():
    """Test FSINFO response format in detail"""

    print("Test: NFS FSINFO Response Format Debug")
    print("=" * 70)
    print()

    host = "localhost"
    port = 2049
    mount_port = portmap_getport(host, 100005, 3)

    # Step 1: MOUNT to get root handle
    print("Step 1: MOUNT /")
    print("-" * 70)
    mount_xid = 600001
    mount_args = pack_string("/")

    reply_data = rpc_call(host, mount_port, mount_xid, 100005, 3, 1, mount_args)

    print(f"  MOUNT response length: {len(reply_data)} bytes")

    # Parse RPC reply header
    if len(reply_data) < 24:
        print(f"  ✗ Response too short: {len(reply_data)} bytes")
        sys.exit(1)

    reply_xid, msg_type, reply_stat, verf_flavor, verf_len, accept_stat = struct.unpack(
        '>IIIIII', reply_data[:24]
    )

    print(f"  RPC reply: xid={reply_xid}, msg_type={msg_type}, reply_stat={reply_stat}, accept_stat={accept_stat}")

    if reply_stat != 0 or accept_stat != 0:
        print(f"  ✗ RPC error: reply_stat={reply_stat}, accept_stat={accept_stat}")
        sys.exit(1)

    offset = 24
    mount_status = struct.unpack('>I', reply_data[offset:offset+4])[0]
    if mount_status != 0:
        print(f"  ✗ MOUNT failed with status {mount_status}")
        sys.exit(1)

    root_fhandle, _ = unpack_opaque_flex(reply_data, offset + 4)
    print(f"  ✓ Got root handle: {len(root_fhandle)} bytes")
    print(f"    Handle (hex): {root_fhandle.hex()}")
    print()

    # Step 2: FSINFO
    print("Step 2: FSINFO (detailed response analysis)")
    print("-" * 70)
    fsinfo_xid = 600002

    # FSINFO3args: fhandle3 (fsroot)
    fsinfo_args = b''
    fsinfo_args += struct.pack('>I', len(root_fhandle)) + root_fhandle
    padding = (4 - (len(root_fhandle) % 4)) % 4
    fsinfo_args += b'\x00' * padding

    print(f"  FSINFO args length: {len(fsinfo_args)} bytes")
    print(f"  FSINFO args (hex): {fsinfo_args.hex()}")
    print()

    reply_data = rpc_call(host, port, fsinfo_xid, 100003, 3, 19, fsinfo_args)

    print(f"  FSINFO response length: {len(reply_data)} bytes")
    print()

    # Show full hex dump
    hex_dump(reply_data, 0, min(len(reply_data), 256))
    print()

    # Parse RPC reply header
    print("  Parsing RPC reply header:")
    if len(reply_data) < 24:
        print(f"    ✗ Response too short: {len(reply_data)} bytes")
        sys.exit(1)

    reply_xid, msg_type, reply_stat, verf_flavor, verf_len, accept_stat = struct.unpack(
        '>IIIIII', reply_data[:24]
    )

    print(f"    xid:         {reply_xid}")
    print(f"    msg_type:    {msg_type}")
    print(f"    reply_stat:  {reply_stat}")
    print(f"    verf_flavor: {verf_flavor}")
    print(f"    verf_len:    {verf_len}")
    print(f"    accept_stat: {accept_stat}")
    print()

    if reply_stat != 0 or accept_stat != 0:
        print(f"  ✗ RPC error: reply_stat={reply_stat}, accept_stat={accept_stat}")
        sys.exit(1)

    offset = 24

    # Parse NFS status
    print("  Parsing NFS response:")
    if len(reply_data) < offset + 4:
        print(f"    ✗ Not enough data for NFS status")
        sys.exit(1)

    nfs_status = struct.unpack('>I', reply_data[offset:offset+4])[0]
    print(f"    NFS status: {nfs_status} (0=NFS3_OK)")
    offset += 4

    if nfs_status != 0:
        print(f"  ✗ FSINFO failed with status {nfs_status}")
        sys.exit(1)

    # Parse post_op_attr discriminator
    print(f"  Parsing post_op_attr (offset={offset}):")
    if len(reply_data) < offset + 4:
        print(f"    ✗ Not enough data for post_op_attr discriminator")
        sys.exit(1)

    attr_follows = struct.unpack('>I', reply_data[offset:offset+4])[0]
    print(f"    attr_follows: {attr_follows} (1=TRUE, 0=FALSE)")
    offset += 4

    if attr_follows == 1:
        # Parse fattr3 (84 bytes)
        print(f"  Parsing fattr3 (offset={offset}):")
        if len(reply_data) < offset + 84:
            print(f"    ✗ Not enough data for fattr3 (need 84 bytes, have {len(reply_data) - offset})")
            sys.exit(1)

        # Just skip fattr3 for now
        hex_dump(reply_data, offset, 84)
        offset += 84
        print(f"    ✓ Skipped fattr3 (84 bytes)")
        print()

    # Parse FSINFO fields
    print(f"  Parsing FSINFO fields (offset={offset}):")

    fields = [
        ("rtmax", 4),
        ("rtpref", 4),
        ("rtmult", 4),
        ("wtmax", 4),
        ("wtpref", 4),
        ("wtmult", 4),
        ("dtpref", 4),
        ("maxfilesize", 8),
        ("time_delta.seconds", 4),
        ("time_delta.nseconds", 4),
        ("properties", 4),
    ]

    values = {}
    for field_name, field_size in fields:
        if len(reply_data) < offset + field_size:
            print(f"    ✗ Not enough data for {field_name} (need {field_size} bytes, have {len(reply_data) - offset})")
            print(f"    Remaining data ({len(reply_data) - offset} bytes):")
            hex_dump(reply_data, offset, len(reply_data) - offset)
            sys.exit(1)

        if field_size == 4:
            value = struct.unpack('>I', reply_data[offset:offset+4])[0]
        elif field_size == 8:
            value = struct.unpack('>Q', reply_data[offset:offset+8])[0]

        values[field_name] = value
        print(f"    {field_name:20} = {value:#x} ({value})")
        offset += field_size

    print()
    print(f"  ✓ Successfully parsed all FSINFO fields!")
    print(f"  Total bytes consumed: {offset}")
    print(f"  Total response length: {len(reply_data)}")
    print()

    # Summary
    print("=" * 70)
    print("✅ FSINFO RESPONSE FORMAT IS CORRECT!")
    print()
    print("Summary:")
    print(f"  Read Transfer:  max={values['rtmax']}, pref={values['rtpref']}, mult={values['rtmult']}")
    print(f"  Write Transfer: max={values['wtmax']}, pref={values['wtpref']}, mult={values['wtmult']}")
    print(f"  Directory pref: {values['dtpref']}")
    print(f"  Max file size:  {values['maxfilesize']:#x}")
    print(f"  Time delta:     {values['time_delta.seconds']}s + {values['time_delta.nseconds']}ns")
    print(f"  Properties:     {values['properties']:#06x}")


if __name__ == '__main__':
    test_fsinfo_format()
