#!/usr/bin/env python3
"""
Test: NFS SETATTR Procedure
Purpose: Test NFS SETATTR to modify file attributes

This test validates:
1. MOUNT to get root directory handle
2. CREATE to create a test file
3. WRITE to add content
4. SETATTR to truncate file
5. READ to verify truncation
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


def parse_wcc_data(reply_data, offset):
    """
    Parse wcc_data structure (RFC 1813)

    wcc_data = {
        pre_op_attr:  bool + optional wcc_attr (24 bytes if present)
        post_op_attr: bool + optional fattr3 (84 bytes if present)
    }

    Returns: (pre_attr_dict, post_attr_dict, next_offset)
    """
    start_offset = offset

    # 1. Parse pre_op_attr
    pre_attr_follows = struct.unpack('>I', reply_data[offset:offset+4])[0]
    offset += 4

    pre_attr = None
    if pre_attr_follows:
        # wcc_attr = size(8) + mtime(8) + ctime(8) = 24 bytes
        size = struct.unpack('>Q', reply_data[offset:offset+8])[0]
        offset += 8
        mtime_sec, mtime_nsec = struct.unpack('>II', reply_data[offset:offset+8])
        offset += 8
        ctime_sec, ctime_nsec = struct.unpack('>II', reply_data[offset:offset+8])
        offset += 8
        pre_attr = {
            'size': size,
            'mtime': (mtime_sec, mtime_nsec),
            'ctime': (ctime_sec, ctime_nsec)
        }

    # 2. Parse post_op_attr
    post_attr_follows = struct.unpack('>I', reply_data[offset:offset+4])[0]
    offset += 4

    post_attr = None
    if post_attr_follows:
        # fattr3 = 84 bytes
        ftype = struct.unpack('>I', reply_data[offset:offset+4])[0]
        mode = struct.unpack('>I', reply_data[offset+4:offset+8])[0]
        nlink = struct.unpack('>I', reply_data[offset+8:offset+12])[0]
        uid = struct.unpack('>I', reply_data[offset+12:offset+16])[0]
        gid = struct.unpack('>I', reply_data[offset+16:offset+20])[0]
        size = struct.unpack('>Q', reply_data[offset+20:offset+28])[0]
        offset += 84

        post_attr = {
            'type': ftype,
            'mode': mode,
            'nlink': nlink,
            'uid': uid,
            'gid': gid,
            'size': size
        }

    # Validate total wcc_data size
    expected_size = 4 + (24 if pre_attr_follows else 0) + 4 + (84 if post_attr_follows else 0)
    actual_size = offset - start_offset
    if actual_size != expected_size:
        raise Exception(f"wcc_data size mismatch: expected {expected_size}, got {actual_size}")

    return pre_attr, post_attr, offset


def test_nfs_setattr():
    """Test NFS SETATTR procedure"""

    print("Test: NFS SETATTR Procedure")
    print("=" * 60)
    print()

    host = "localhost"
    port = 4000

    # Test file
    test_filename = "test_setattr_file.txt"
    test_data = b"Hello, this is test content for SETATTR!"
    print(f"Test file: {test_filename}")
    print(f"Initial content: {test_data}")
    print()

    # Step 1: MOUNT
    print("Step 1: MOUNT /")
    print("-" * 60)
    mount_xid = 700001
    mount_args = pack_string("/")

    reply_data = rpc_call(host, port, mount_xid, 100005, 3, 1, mount_args)
    offset = parse_rpc_reply(reply_data)

    mount_status = struct.unpack('>I', reply_data[offset:offset+4])[0]
    if mount_status != 0:
        print(f"  ✗ MOUNT failed with status {mount_status}")
        sys.exit(1)

    root_fhandle, _ = unpack_opaque_flex(reply_data, offset + 4)
    print(f"  ✓ Got root handle: {len(root_fhandle)} bytes")
    print()

    # Step 2: CREATE file
    print(f"Step 2: CREATE {test_filename}")
    print("-" * 60)
    create_xid = 700002

    create_args = b''
    create_args += struct.pack('>I', len(root_fhandle)) + root_fhandle
    padding = (4 - (len(root_fhandle) % 4)) % 4
    create_args += b'\x00' * padding
    create_args += pack_string(test_filename)

    # createhow3: UNCHECKED (0) + sattr3
    create_args += struct.pack('>I', 0)     # UNCHECKED
    # sattr3 (union format):
    create_args += struct.pack('>I', 1)     # mode discriminator = SET_MODE
    create_args += struct.pack('>I', 0o644) # mode value
    create_args += struct.pack('>I', 0)     # uid discriminator = DONT_SET_UID
    create_args += struct.pack('>I', 0)     # gid discriminator = DONT_SET_GID
    create_args += struct.pack('>I', 0)     # size discriminator = DONT_SET_SIZE
    create_args += struct.pack('>I', 0)     # atime discriminator = DONT_CHANGE
    create_args += struct.pack('>I', 0)     # mtime discriminator = DONT_CHANGE

    reply_data = rpc_call(host, port, create_xid, 100003, 3, 8, create_args)
    offset = parse_rpc_reply(reply_data)

    nfs_status = struct.unpack('>I', reply_data[offset:offset+4])[0]
    if nfs_status != 0:
        print(f"  ✗ CREATE failed with status {nfs_status}")
        sys.exit(1)

    offset += 4
    handle_follows = struct.unpack('>I', reply_data[offset:offset+4])[0]
    offset += 4

    if handle_follows:
        file_handle, _ = unpack_opaque_flex(reply_data, offset)
        print(f"  ✓ Created file, handle: {len(file_handle)} bytes")
    else:
        print(f"  ✗ No file handle returned")
        sys.exit(1)
    print()

    # Step 3: WRITE initial content
    print(f"Step 3: WRITE {len(test_data)} bytes")
    print("-" * 60)
    write_xid = 700003

    write_args = b''
    write_args += struct.pack('>I', len(file_handle)) + file_handle
    padding = (4 - (len(file_handle) % 4)) % 4
    write_args += b'\x00' * padding
    write_args += struct.pack('>Q', 0)                  # offset
    write_args += struct.pack('>I', len(test_data))     # count
    write_args += struct.pack('>I', 2)                  # stable = FILE_SYNC
    write_args += struct.pack('>I', len(test_data)) + test_data
    data_padding = (4 - (len(test_data) % 4)) % 4
    write_args += b'\x00' * data_padding

    reply_data = rpc_call(host, port, write_xid, 100003, 3, 7, write_args)
    offset = parse_rpc_reply(reply_data)

    nfs_status = struct.unpack('>I', reply_data[offset:offset+4])[0]
    if nfs_status != 0:
        print(f"  ✗ WRITE failed with status {nfs_status}")
        sys.exit(1)

    print(f"  ✓ Wrote {len(test_data)} bytes")
    print()

    # Step 4: SETATTR to truncate to 5 bytes
    print(f"Step 4: SETATTR to truncate file to 5 bytes")
    print("-" * 60)
    setattr_xid = 700004
    new_size = 5

    # SETATTR3args: file handle + new_attributes (sattr3) + guard (sattrguard3)
    setattr_args = b''

    # File handle
    setattr_args += struct.pack('>I', len(file_handle)) + file_handle
    padding = (4 - (len(file_handle) % 4)) % 4
    setattr_args += b'\x00' * padding

    # sattr3: new_attributes (union format)
    setattr_args += struct.pack('>I', 0)     # mode discriminator = DONT_SET_MODE
    setattr_args += struct.pack('>I', 0)     # uid discriminator = DONT_SET_UID
    setattr_args += struct.pack('>I', 0)     # gid discriminator = DONT_SET_GID
    setattr_args += struct.pack('>I', 1)     # size discriminator = SET_SIZE
    setattr_args += struct.pack('>Q', new_size)  # size value = 5
    setattr_args += struct.pack('>I', 0)     # atime discriminator = DONT_CHANGE
    setattr_args += struct.pack('>I', 0)     # mtime discriminator = DONT_CHANGE

    # sattrguard3: guard
    setattr_args += struct.pack('>I', 0)     # check = FALSE
    setattr_args += struct.pack('>I', 0)     # obj_ctime.seconds
    setattr_args += struct.pack('>I', 0)     # obj_ctime.nseconds

    print(f"  Setting size to {new_size} bytes")

    reply_data = rpc_call(host, port, setattr_xid, 100003, 3, 2, setattr_args)
    offset = parse_rpc_reply(reply_data)

    # Parse SETATTR3res (RFC 1813)
    # SETATTR3res = nfsstat3 + wcc_data (obj_wcc)
    nfs_status = struct.unpack('>I', reply_data[offset:offset+4])[0]
    offset += 4

    print(f"  NFS status: {nfs_status} (0=NFS3_OK)")

    if nfs_status != 0:
        print(f"  ✗ SETATTR failed with status {nfs_status}")
        sys.exit(1)

    # Parse wcc_data (obj_wcc - file's weak cache consistency data)
    pre_attr, post_attr, offset = parse_wcc_data(reply_data, offset)

    if post_attr:
        print(f"  ✓ File post_op_attr: size={post_attr['size']}")
        if post_attr['size'] == new_size:
            print(f"    ✅ Confirmed size changed to {new_size} bytes")

    # Validate exact response length
    expected_rpc_header = 24
    expected_nfs_status = 4
    expected_wcc_data = 4 + (24 if pre_attr else 0) + 4 + (84 if post_attr else 0)
    expected_total = expected_rpc_header + expected_nfs_status + expected_wcc_data

    if len(reply_data) != expected_total:
        raise Exception(f"SETATTR response length mismatch: expected {expected_total}, got {len(reply_data)}")

    print(f"  ✓ Response format validation passed (length={len(reply_data)} bytes)")
    print(f"  ✓ SETATTR succeeded")
    print()

    # Step 5: READ to verify truncation
    print(f"Step 5: READ to verify file was truncated")
    print("-" * 60)
    read_xid = 700005

    read_args = b''
    read_args += struct.pack('>I', len(file_handle)) + file_handle
    padding = (4 - (len(file_handle) % 4)) % 4
    read_args += b'\x00' * padding
    read_args += struct.pack('>Q', 0)        # offset = 0
    read_args += struct.pack('>I', 1024)     # count = 1024

    reply_data = rpc_call(host, port, read_xid, 100003, 3, 6, read_args)
    offset = parse_rpc_reply(reply_data)

    nfs_status = struct.unpack('>I', reply_data[offset:offset+4])[0]
    if nfs_status != 0:
        print(f"  ✗ READ failed with status {nfs_status}")
        sys.exit(1)

    # Parse READ3resok
    offset += 4
    attr_follows = struct.unpack('>I', reply_data[offset:offset+4])[0]
    offset += 4
    if attr_follows:
        offset += 84  # Skip fattr3

    read_count = struct.unpack('>I', reply_data[offset:offset+4])[0]
    offset += 4
    eof = struct.unpack('>I', reply_data[offset:offset+4])[0]
    offset += 4
    data_length = struct.unpack('>I', reply_data[offset:offset+4])[0]
    offset += 4
    read_data = reply_data[offset:offset+data_length]

    print(f"  Read {read_count} bytes")
    print(f"  Data: {read_data}")

    expected_data = test_data[:new_size]
    if read_data == expected_data:
        print(f"  ✅ Verified: File truncated correctly to {new_size} bytes")
        print(f"  Expected: {expected_data}")
    else:
        print(f"  ✗ Data mismatch!")
        print(f"    Expected: {expected_data}")
        print(f"    Got:      {read_data}")
        sys.exit(1)

    print()
    print("=" * 60)
    print("✅ NFS SETATTR test PASSED")
    print()
    print("Summary:")
    print("  ✓ CREATE file succeeded")
    print("  ✓ WRITE initial content succeeded")
    print("  ✓ SETATTR truncate succeeded")
    print("  ✓ READ verified truncation")


if __name__ == '__main__':
    test_nfs_setattr()
