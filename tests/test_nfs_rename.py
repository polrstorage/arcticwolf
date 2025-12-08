#!/usr/bin/env python3
"""
Test NFS RENAME Procedure (14)

Tests the NFSv3 RENAME operation which renames or moves files/directories.
"""

import socket
import struct
import sys


def pack_rpc_call(xid, prog, vers, proc, auth_flavor=0, auth_len=0, verf_flavor=0, verf_len=0):
    """Pack RPC call header"""
    rpc_header = struct.pack(
        '>IIIIIIII',
        xid,         # XID
        0,           # Message type (0 = CALL)
        2,           # RPC version
        prog,        # Program
        vers,        # Version
        proc,        # Procedure
        auth_flavor, # Auth flavor
        auth_len     # Auth length
    )

    verf = struct.pack('>II', verf_flavor, verf_len)
    return rpc_header + verf


def pack_fhandle3(handle):
    """Pack file handle (length + data + padding)"""
    handle_len = len(handle)
    packed = struct.pack('>I', handle_len)
    packed += handle
    padding = (4 - (handle_len % 4)) % 4
    packed += b'\x00' * padding
    return packed


def pack_filename3(name):
    """Pack filename (length + string + padding)"""
    name_bytes = name.encode('utf-8')
    name_len = len(name_bytes)
    packed = struct.pack('>I', name_len)
    packed += name_bytes
    padding = (4 - (name_len % 4)) % 4
    packed += b'\x00' * padding
    return packed


def pack_rename3args(from_dir_handle, from_name, to_dir_handle, to_name):
    """Pack RENAME3args structure"""
    packed = pack_fhandle3(from_dir_handle)
    packed += pack_filename3(from_name)
    packed += pack_fhandle3(to_dir_handle)
    packed += pack_filename3(to_name)
    return packed


def unpack_opaque_flex(data, offset):
    """Unpack variable-length opaque data (length + data + padding)"""
    length = struct.unpack('>I', data[offset:offset+4])[0]
    offset += 4
    opaque_data = data[offset:offset+length]
    padding = (4 - (length % 4)) % 4
    offset += length + padding
    return opaque_data, offset


def parse_wcc_data(reply_data, offset):
    """
    Parse wcc_data structure (RFC 1813 Section 3.3.6)

    wcc_data = {
        before: pre_op_attr   (bool + optional 24 bytes)
        after:  post_op_attr  (bool + optional 84 bytes)
    }
    """
    start_offset = offset

    # Parse pre_op_attr
    pre_attr_follows = struct.unpack('>I', reply_data[offset:offset+4])[0]
    offset += 4

    if pre_attr_follows:
        # wcc_attr = 24 bytes (size:8 + mtime:8 + ctime:8)
        size = struct.unpack('>Q', reply_data[offset:offset+8])[0]
        offset += 8
        mtime_sec, mtime_nsec = struct.unpack('>II', reply_data[offset:offset+8])
        offset += 8
        ctime_sec, ctime_nsec = struct.unpack('>II', reply_data[offset:offset+8])
        offset += 8

    # Parse post_op_attr
    post_attr_follows = struct.unpack('>I', reply_data[offset:offset+4])[0]
    offset += 4

    if post_attr_follows:
        # fattr3 = 84 bytes
        offset += 84

    # Validate wcc_data size
    expected_size = 4 + (24 if pre_attr_follows else 0) + 4 + (84 if post_attr_follows else 0)
    actual_size = offset - start_offset

    if actual_size != expected_size:
        raise Exception(f"wcc_data size mismatch: expected {expected_size}, got {actual_size}")

    return offset


def test_rename_file(server_ip, server_port):
    """Test renaming a file"""

    print("=" * 60)
    print("Testing NFS RENAME Procedure (14) - File Rename")
    print("=" * 60)

    sock = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
    sock.connect((server_ip, server_port))

    try:
        # Step 1: MOUNT to get root handle
        print("\n[1] Getting root handle via MOUNT...")
        xid = 0x12345680
        rpc_call = pack_rpc_call(xid, 100005, 3, 1)  # MOUNT (proc 1)
        mount_args = pack_filename3("/")

        msg = rpc_call + mount_args
        record_marker = struct.pack('>I', 0x80000000 | len(msg))
        sock.send(record_marker + msg)

        header = sock.recv(4)
        response_len = struct.unpack('>I', header)[0] & 0x7FFFFFFF
        reply_data = sock.recv(response_len)

        offset = 24
        status = struct.unpack('>I', reply_data[offset:offset+4])[0]
        if status != 0:
            print(f"  ERROR: MOUNT failed")
            return False

        root_handle, _ = unpack_opaque_flex(reply_data, offset + 4)
        print(f"  Got root handle: {root_handle.hex()} ({len(root_handle)} bytes)")

        # Step 2: Create a test file using CREATE
        print("\n[2] Creating test file 'oldname.txt'...")
        xid = 0x12345681
        rpc_call = pack_rpc_call(xid, 100003, 3, 8)  # CREATE (proc 8)

        # CREATE3args: dir handle + filename + how (createhow3 = UNCHECKED + sattr3)
        create_args = pack_fhandle3(root_handle)
        create_args += pack_filename3("oldname.txt")
        create_args += struct.pack('>I', 0)  # UNCHECKED mode
        # sattr3: all fields set to DONT_SET (discriminator = 0)
        for _ in range(6):  # mode, uid, gid, size, atime, mtime
            create_args += struct.pack('>I', 0)

        msg = rpc_call + create_args
        record_marker = struct.pack('>I', 0x80000000 | len(msg))
        sock.send(record_marker + msg)

        header = sock.recv(4)
        response_len = struct.unpack('>I', header)[0] & 0x7FFFFFFF
        reply_data = sock.recv(response_len)

        offset = 24
        status = struct.unpack('>I', reply_data[offset:offset+4])[0]
        if status != 0:
            print(f"  ERROR: CREATE failed with status {status}")
            return False
        print(f"  File created successfully")

        # Step 3: Rename the file
        print("\n[3] Renaming 'oldname.txt' to 'newname.txt'...")
        xid = 0x12345682
        rpc_call = pack_rpc_call(xid, 100003, 3, 14)  # RENAME (proc 14)

        rename_args = pack_rename3args(root_handle, "oldname.txt", root_handle, "newname.txt")

        msg = rpc_call + rename_args
        record_marker = struct.pack('>I', 0x80000000 | len(msg))
        sock.send(record_marker + msg)

        header = sock.recv(4)
        response_len = struct.unpack('>I', header)[0] & 0x7FFFFFFF
        reply_data = sock.recv(response_len)

        # Parse RPC reply header
        (reply_xid, msg_type, reply_stat, verf_flavor, verf_len, accept_stat) = \
            struct.unpack('>IIIIII', reply_data[:24])

        print(f"  RENAME XID: {hex(reply_xid)}, accept_stat: {accept_stat}")

        # Parse RENAME3res
        offset = 24
        status = struct.unpack('>I', reply_data[offset:offset+4])[0]
        offset += 4

        print(f"  Status: {status} (0=NFS3_OK)")

        if status != 0:
            print(f"  ERROR: RENAME failed with status {status}")
            # Still parse wcc_data for failure case
            print(f"\n  Parsing fromdir_wcc...")
            offset = parse_wcc_data(reply_data, offset)
            print(f"  Parsing todir_wcc...")
            offset = parse_wcc_data(reply_data, offset)
            return False

        # Success case: parse fromdir_wcc + todir_wcc
        print(f"\n  Parsing RENAME3resok structure...")

        # Parse fromdir_wcc (source directory wcc_data)
        print(f"  Parsing fromdir_wcc (source directory)...")
        offset = parse_wcc_data(reply_data, offset)

        # Parse todir_wcc (target directory wcc_data)
        print(f"  Parsing todir_wcc (target directory)...")
        offset = parse_wcc_data(reply_data, offset)

        print(f"\n  Total response size: {len(reply_data)} bytes")
        print(f"  Parsed offset: {offset} bytes")

        if offset != len(reply_data):
            print(f"  WARNING: Response size mismatch!")
            print(f"  Unparsed data: {(len(reply_data) - offset)} bytes")
            return False

        print("\n✓ RENAME file test PASSED - format validation successful")
        return True

    finally:
        sock.close()


def test_rename_nonexistent(server_ip, server_port):
    """Test RENAME on non-existent file (should return NFS3ERR_NOENT)"""

    print("\n" + "=" * 60)
    print("Testing RENAME on non-existent file")
    print("=" * 60)

    sock = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
    sock.connect((server_ip, server_port))

    try:
        # First get root handle via MOUNT
        print("\n[1] Getting root handle via MOUNT...")
        xid = 0x12345683
        rpc_call = pack_rpc_call(xid, 100005, 3, 1)  # MOUNT (proc 1)
        mount_args = pack_filename3("/")

        msg = rpc_call + mount_args
        record_marker = struct.pack('>I', 0x80000000 | len(msg))
        sock.send(record_marker + msg)

        header = sock.recv(4)
        response_len = struct.unpack('>I', header)[0] & 0x7FFFFFFF
        reply_data = sock.recv(response_len)

        offset = 24
        status = struct.unpack('>I', reply_data[offset:offset+4])[0]
        if status != 0:
            print(f"  ERROR: MOUNT failed")
            return False

        root_handle, _ = unpack_opaque_flex(reply_data, offset + 4)

        print("\n[2] Attempting to rename non-existent file 'nosuchfile.txt'...")
        xid = 0x12345684
        rpc_call = pack_rpc_call(xid, 100003, 3, 14)  # RENAME (proc 14)

        rename_args = pack_rename3args(root_handle, "nosuchfile.txt", root_handle, "renamed.txt")

        msg = rpc_call + rename_args
        record_marker = struct.pack('>I', 0x80000000 | len(msg))
        sock.send(record_marker + msg)

        # Receive response
        header = sock.recv(4)
        response_len = struct.unpack('>I', header)[0] & 0x7FFFFFFF
        reply_data = sock.recv(response_len)

        # Parse response
        offset = 24  # Skip RPC header
        status = struct.unpack('>I', reply_data[offset:offset+4])[0]
        offset += 4

        print(f"  Status: {status} (2=NFS3ERR_NOENT expected)")

        # Parse fromdir_wcc and todir_wcc (present in both success and failure cases)
        print(f"\n  Parsing fromdir_wcc...")
        offset = parse_wcc_data(reply_data, offset)
        print(f"  Parsing todir_wcc...")
        offset = parse_wcc_data(reply_data, offset)

        if offset != len(reply_data):
            print(f"  WARNING: Response size mismatch!")
            return False

        if status == 2:  # NFS3ERR_NOENT
            print("\n✓ RENAME non-existent test PASSED - returned NFS3ERR_NOENT")
            return True
        else:
            print(f"\n✗ RENAME test FAILED - expected status 2, got {status}")
            return False

    finally:
        sock.close()


if __name__ == '__main__':
    if len(sys.argv) != 3:
        print(f"Usage: {sys.argv[0]} <server_ip> <server_port>")
        sys.exit(1)

    server_ip = sys.argv[1]
    server_port = int(sys.argv[2])

    success = True

    # Test 1: Rename file
    if not test_rename_file(server_ip, server_port):
        success = False

    # Test 2: Try to rename non-existent file
    if not test_rename_nonexistent(server_ip, server_port):
        success = False

    if success:
        print("\n" + "=" * 60)
        print("ALL TESTS PASSED ✓")
        print("=" * 60)
        sys.exit(0)
    else:
        print("\n" + "=" * 60)
        print("SOME TESTS FAILED ✗")
        print("=" * 60)
        sys.exit(1)
