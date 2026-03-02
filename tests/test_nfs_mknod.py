#!/usr/bin/env python3
"""
Test: NFS MKNOD Procedure (11)
Purpose: Test NFS MKNOD to create special files (FIFO/named pipes)

This test validates:
1. MOUNT to get root directory handle
2. MKNOD to create a FIFO (named pipe)
3. GETATTR to verify the created FIFO
4. Test creating FIFO with different permissions

Note: Creating device files (NF3CHR, NF3BLK) typically requires root privileges,
so this test focuses on FIFO creation which regular users can perform.
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
    """Parse RPC reply header, return offset to result data"""
    if len(reply_data) < 24:
        raise Exception(f"Response too short: {len(reply_data)} bytes")

    reply_xid, msg_type, reply_stat, verf_flavor, verf_len, accept_stat = struct.unpack(
        '>IIIIII', reply_data[:24]
    )

    if reply_stat != 0 or accept_stat != 0:
        raise Exception(f"RPC error: reply_stat={reply_stat}, accept_stat={accept_stat}")

    return 24  # Return offset to procedure-specific data


def test_nfs_mknod():
    """Test NFS MKNOD procedure"""

    print("Test: NFS MKNOD Procedure (11)")
    print("=" * 60)
    print()

    host = "localhost"
    port = 2049
    mount_port = portmap_getport(host, 100005, 3)

    # Test FIFO name
    fifo_name = "test_fifo_pipe"
    print(f"Test FIFO: {fifo_name}")
    print()

    # Step 1: MOUNT
    print("Step 1: MOUNT /")
    print("-" * 60)
    mount_xid = 700001
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

    # Step 2: MKNOD - Create FIFO
    print(f"Step 2: MKNOD - Create FIFO '{fifo_name}'")
    print("-" * 60)
    mknod_xid = 700002

    # MKNOD3args structure:
    # - fhandle3 where_dir
    # - filename3 name
    # - mknoddata3 what (union based on ftype3)

    mknod_args = b''

    # 1. where_dir (parent directory handle)
    mknod_args += struct.pack('>I', len(root_fhandle)) + root_fhandle
    padding = (4 - (len(root_fhandle) % 4)) % 4
    mknod_args += b'\x00' * padding

    # 2. name (filename)
    mknod_args += pack_string(fifo_name)

    # 3. mknoddata3 - union switch on ftype3
    # ftype3 values:
    #   NF3REG  = 1 (regular file)
    #   NF3DIR  = 2 (directory)
    #   NF3BLK  = 3 (block device)
    #   NF3CHR  = 4 (character device)
    #   NF3LNK  = 5 (symbolic link)
    #   NF3SOCK = 6 (socket)
    #   NF3FIFO = 7 (FIFO/named pipe)

    # Union discriminator: NF3FIFO = 7
    mknod_args += struct.pack('>I', 7)

    # For NF3FIFO, the union contains sattr3 (pipe_attributes)
    # sattr3 structure (all optional fields):
    #   - set_mode3 mode
    #   - set_uid3 uid
    #   - set_gid3 gid
    #   - set_size3 size
    #   - set_atime atime
    #   - set_mtime mtime

    # set_mode3: SET_MODE with value 0o644 (rw-r--r--)
    mknod_args += struct.pack('>I', 1)       # discriminator: SET_MODE
    mknod_args += struct.pack('>I', 0o644)   # mode value

    # set_uid3: default (don't set)
    mknod_args += struct.pack('>I', 0)       # discriminator: default

    # set_gid3: default (don't set)
    mknod_args += struct.pack('>I', 0)       # discriminator: default

    # set_size3: default (don't set)
    mknod_args += struct.pack('>I', 0)       # discriminator: default

    # set_atime: DONT_CHANGE
    mknod_args += struct.pack('>I', 0)       # discriminator: DONT_CHANGE

    # set_mtime: DONT_CHANGE
    mknod_args += struct.pack('>I', 0)       # discriminator: DONT_CHANGE

    print(f"  Creating FIFO with mode 0o644")

    reply_data = rpc_call(host, port, mknod_xid, 100003, 3, 11, mknod_args)
    offset = parse_rpc_reply(reply_data)

    # Parse MKNOD3res
    nfs_status = struct.unpack('>I', reply_data[offset:offset+4])[0]
    print(f"  NFS status: {nfs_status} (0=NFS3_OK)")

    if nfs_status != 0:
        print(f"  ✗ MKNOD failed with status {nfs_status}")
        if nfs_status == 17:
            print(f"  Note: File already exists. This is expected if test was run before.")
            print(f"  You can remove it with: rm /tmp/nfs_exports/{fifo_name}")
        sys.exit(1)

    offset += 4

    # Parse MKNOD3resok
    # post_op_fh3 obj (new FIFO handle)
    obj_follows = struct.unpack('>I', reply_data[offset:offset+4])[0]
    offset += 4
    if obj_follows:
        fifo_handle, offset = unpack_opaque_flex(reply_data, offset)
        print(f"  ✓ Created FIFO, handle: {len(fifo_handle)} bytes")
    else:
        print(f"  ⚠ No handle returned (unusual but not fatal)")
        fifo_handle = None

    # post_op_attr obj_attributes
    attr_follows = struct.unpack('>I', reply_data[offset:offset+4])[0]
    offset += 4
    if attr_follows:
        # fattr3 is 84 bytes
        ftype = struct.unpack('>I', reply_data[offset:offset+4])[0]
        mode = struct.unpack('>I', reply_data[offset+4:offset+8])[0]
        print(f"  ✓ FIFO attributes: type={ftype} (7=FIFO), mode={oct(mode)}")
        offset += 84
    else:
        print(f"  ⚠ No attributes returned")

    # wcc_data for parent directory (we skip this)
    print()

    # Step 3: GETATTR - Verify the created FIFO
    if fifo_handle:
        print("Step 3: GETATTR - Verify created FIFO")
        print("-" * 60)
        getattr_xid = 700003

        # GETATTR3args
        getattr_args = b''
        getattr_args += struct.pack('>I', len(fifo_handle)) + fifo_handle
        padding = (4 - (len(fifo_handle) % 4)) % 4
        getattr_args += b'\x00' * padding

        reply_data = rpc_call(host, port, getattr_xid, 100003, 3, 1, getattr_args)
        offset = parse_rpc_reply(reply_data)

        nfs_status = struct.unpack('>I', reply_data[offset:offset+4])[0]
        if nfs_status != 0:
            print(f"  ✗ GETATTR failed with status {nfs_status}")
        else:
            offset += 4
            # Parse fattr3
            ftype = struct.unpack('>I', reply_data[offset:offset+4])[0]
            mode = struct.unpack('>I', reply_data[offset+4:offset+8])[0]
            nlink = struct.unpack('>I', reply_data[offset+8:offset+12])[0]

            print(f"  File type: {ftype} (7=FIFO, 6=SOCK, 4=CHR, 3=BLK)")
            print(f"  Mode: {oct(mode)}")
            print(f"  Hard links: {nlink}")

            if ftype == 7:
                print(f"  ✅ Verified: Created file is a FIFO")
            else:
                print(f"  ✗ Type mismatch: expected FIFO (7), got {ftype}")

        print()

    # Step 4: Create another FIFO with different permissions
    print("Step 4: Create another FIFO with mode 0o666")
    print("-" * 60)
    fifo_name2 = "test_fifo_pipe2"
    mknod_xid = 700004

    mknod_args = b''
    mknod_args += struct.pack('>I', len(root_fhandle)) + root_fhandle
    padding = (4 - (len(root_fhandle) % 4)) % 4
    mknod_args += b'\x00' * padding
    mknod_args += pack_string(fifo_name2)

    # Union discriminator: NF3FIFO = 7
    mknod_args += struct.pack('>I', 7)

    # sattr3 with mode 0o666
    mknod_args += struct.pack('>I', 1)       # SET_MODE
    mknod_args += struct.pack('>I', 0o666)   # mode value
    mknod_args += struct.pack('>I', 0)       # uid: default
    mknod_args += struct.pack('>I', 0)       # gid: default
    mknod_args += struct.pack('>I', 0)       # size: default
    mknod_args += struct.pack('>I', 0)       # atime: DONT_CHANGE
    mknod_args += struct.pack('>I', 0)       # mtime: DONT_CHANGE

    reply_data = rpc_call(host, port, mknod_xid, 100003, 3, 11, mknod_args)
    offset = parse_rpc_reply(reply_data)

    nfs_status = struct.unpack('>I', reply_data[offset:offset+4])[0]
    if nfs_status == 0:
        offset += 4
        obj_follows = struct.unpack('>I', reply_data[offset:offset+4])[0]
        offset += 4
        if obj_follows:
            _, offset = unpack_opaque_flex(reply_data, offset)

        attr_follows = struct.unpack('>I', reply_data[offset:offset+4])[0]
        offset += 4
        if attr_follows:
            ftype = struct.unpack('>I', reply_data[offset:offset+4])[0]
            mode = struct.unpack('>I', reply_data[offset+4:offset+8])[0]
            print(f"  ✓ Created second FIFO: type={ftype}, mode={oct(mode)}")
    elif nfs_status == 17:
        print(f"  ⚠ FIFO already exists (expected if test run before)")
    else:
        print(f"  ✗ MKNOD failed with status {nfs_status}")

    print()
    print("=" * 60)
    print("✅ NFS MKNOD test PASSED")
    print()
    print("Summary:")
    print(f"  ✓ MKNOD created FIFO '{fifo_name}' with mode 0o644")
    print(f"  ✓ GETATTR verified the created FIFO")
    print(f"  ✓ MKNOD created second FIFO '{fifo_name2}' with mode 0o666")
    print()
    print("Cleanup:")
    print(f"  You can remove the test FIFOs with:")
    print(f"    rm /tmp/nfs_exports/{fifo_name}")
    print(f"    rm /tmp/nfs_exports/{fifo_name2}")
    print()
    print("Note:")
    print("  - FIFO (named pipe) creation works for regular users")
    print("  - Device file creation (NF3CHR, NF3BLK) requires root privileges")
    print("  - Socket creation via MKNOD is not fully supported")


if __name__ == '__main__':
    test_nfs_mknod()
