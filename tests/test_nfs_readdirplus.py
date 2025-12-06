#!/usr/bin/env python3
"""
Test: NFS READDIRPLUS Procedure
Purpose: Test directory listing with attributes and handles
"""

import socket
import struct
import sys


def pack_string(s):
    """Pack a string as XDR string"""
    data = s.encode('utf-8')
    length = len(data)
    padding = (4 - (length % 4)) % 4
    return struct.pack('>I', length) + data + b'\x00' * padding


def unpack_string(data, offset):
    """Unpack XDR string"""
    length = struct.unpack('>I', data[offset:offset+4])[0]
    string_data = data[offset+4:offset+4+length].decode('utf-8')
    padding = (4 - (length % 4)) % 4
    next_offset = offset + 4 + length + padding
    return string_data, next_offset


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

    return 24  # Return offset to NFS data


def test_readdirplus(host, port):
    """Test READDIRPLUS procedure"""
    print("\n=== Test: NFS READDIRPLUS ===")

    # Step 1: Mount to get root file handle
    print("\n1. Calling MOUNT...")
    mount_args = pack_string("/tmp/nfs_exports")
    mount_reply = rpc_call(host, port, 1, 100005, 3, 1, mount_args)
    offset = parse_rpc_reply(mount_reply)

    # Parse MOUNT reply
    status = struct.unpack('>I', mount_reply[offset:offset+4])[0]
    if status != 0:
        raise Exception(f"MOUNT failed with status {status}")
    offset += 4

    root_handle, offset = unpack_opaque_flex(mount_reply, offset)
    print(f"  Got root handle: {len(root_handle)} bytes")

    # Step 2: READDIRPLUS (procedure 17)
    print("\n2. Calling READDIRPLUS...")
    readdirplus_args = b''

    # dir (fhandle3)
    readdirplus_args += struct.pack('>I', len(root_handle))
    readdirplus_args += root_handle
    padding = (4 - (len(root_handle) % 4)) % 4
    readdirplus_args += b'\x00' * padding

    # cookie (uint64)
    readdirplus_args += struct.pack('>Q', 0)

    # cookieverf (8 bytes)
    readdirplus_args += b'\x00' * 8

    # dircount (uint32) - max bytes for directory entries
    readdirplus_args += struct.pack('>I', 8192)

    # maxcount (uint32) - max bytes for entire response
    readdirplus_args += struct.pack('>I', 32768)

    readdirplus_reply = rpc_call(host, port, 2, 100003, 3, 17, readdirplus_args)
    offset = parse_rpc_reply(readdirplus_reply)

    # Parse READDIRPLUS reply
    # status
    status = struct.unpack('>I', readdirplus_reply[offset:offset+4])[0]
    offset += 4

    if status != 0:
        raise Exception(f"READDIRPLUS failed with status {status}")

    print(f"  READDIRPLUS status: NFS3_OK (0)")

    # post_op_attr (dir_attributes)
    attr_follows = struct.unpack('>I', readdirplus_reply[offset:offset+4])[0]
    offset += 4

    if attr_follows:
        print(f"  Directory attributes present")
        # Skip fattr3 (84 bytes)
        offset += 84

    # cookieverf (8 bytes)
    offset += 8

    # Parse entryplus3 list
    print(f"\n  Directory entries:")
    entry_count = 0
    while True:
        # Check if there's more data
        value_follows = struct.unpack('>I', readdirplus_reply[offset:offset+4])[0]
        offset += 4

        if value_follows == 0:
            # No more entries
            break

        # Parse entryplus3
        # fileid (uint64)
        fileid = struct.unpack('>Q', readdirplus_reply[offset:offset+8])[0]
        offset += 8

        # name (string)
        name, offset = unpack_string(readdirplus_reply, offset)

        # cookie (uint64)
        cookie = struct.unpack('>Q', readdirplus_reply[offset:offset+8])[0]
        offset += 8

        # post_op_attr (name_attributes)
        attr_follows = struct.unpack('>I', readdirplus_reply[offset:offset+4])[0]
        offset += 4

        if attr_follows:
            # fattr3 structure (84 bytes)
            # type (4) + mode (4) + nlink (4) + uid (4) + gid (4) +
            # size (8) + used (8) + rdev (8) + fsid (8) + fileid (8) +
            # atime (8) + mtime (8) + ctime (8)
            ftype = struct.unpack('>I', readdirplus_reply[offset:offset+4])[0]
            mode = struct.unpack('>I', readdirplus_reply[offset+4:offset+8])[0]
            size = struct.unpack('>Q', readdirplus_reply[offset+20:offset+28])[0]
            offset += 84

            type_names = {1: "REG", 2: "DIR", 3: "BLK", 4: "CHR", 5: "LNK", 6: "SOCK", 7: "FIFO"}
            type_str = type_names.get(ftype, f"UNKNOWN({ftype})")

            print(f"    - {name:20s} (fileid={fileid}, type={type_str}, mode={oct(mode)}, size={size})")
        else:
            print(f"    - {name:20s} (fileid={fileid}, no attributes)")

        # post_op_fh3 (name_handle)
        handle_follows = struct.unpack('>I', readdirplus_reply[offset:offset+4])[0]
        offset += 4

        if handle_follows:
            handle, offset = unpack_opaque_flex(readdirplus_reply, offset)
            print(f"      Handle: {len(handle)} bytes")

        entry_count += 1

    # eof
    eof = struct.unpack('>I', readdirplus_reply[offset:offset+4])[0]
    offset += 4

    print(f"\n  Total entries: {entry_count}")
    print(f"  EOF: {bool(eof)}")
    print(f"  Total response size: {len(readdirplus_reply)} bytes")

    if entry_count == 0:
        raise Exception("Expected at least some entries (. and ..)")

    print("\n✓ READDIRPLUS test passed!")


if __name__ == '__main__':
    host = sys.argv[1] if len(sys.argv) > 1 else '127.0.0.1'
    port = int(sys.argv[2]) if len(sys.argv) > 2 else 2049

    try:
        test_readdirplus(host, port)
    except Exception as e:
        print(f"\n✗ Test failed: {e}", file=sys.stderr)
        import traceback
        traceback.print_exc()
        sys.exit(1)
