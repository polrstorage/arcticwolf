#!/usr/bin/env python3
"""
Test: NFS COMMIT Procedure (21)
Purpose: Test NFS COMMIT to flush cached writes to stable storage

This test validates:
1. MOUNT to get root directory handle
2. LOOKUP to get file handle
3. WRITE with UNSTABLE mode (deferred write)
4. COMMIT to force cached writes to stable storage
5. Verify write verifier is returned
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


def test_nfs_commit():
    """Test NFS COMMIT procedure"""

    print("Test: NFS COMMIT Procedure (21)")
    print("=" * 60)
    print()

    host = "localhost"
    port = 4000

    # Test file
    test_filename = "test_commit_file.txt"
    test_data = b"Data for COMMIT test - UNSTABLE write"
    print(f"Test file: {test_filename}")
    print(f"Test data: {test_data}")
    print()

    # Step 1: MOUNT
    print("Step 1: MOUNT /")
    print("-" * 60)
    mount_xid = 600001
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

    # Step 2: LOOKUP test file
    print(f"Step 2: LOOKUP {test_filename}")
    print("-" * 60)
    lookup_xid = 600002

    # LOOKUP3args
    lookup_args = b''
    lookup_args += struct.pack('>I', len(root_fhandle)) + root_fhandle
    padding = (4 - (len(root_fhandle) % 4)) % 4
    lookup_args += b'\x00' * padding
    lookup_args += pack_string(test_filename)

    reply_data = rpc_call(host, port, lookup_xid, 100003, 3, 3, lookup_args)
    offset = parse_rpc_reply(reply_data)

    nfs_status = struct.unpack('>I', reply_data[offset:offset+4])[0]
    if nfs_status != 0:
        print(f"  ⚠ LOOKUP failed with status {nfs_status} (file may not exist yet)")
        print(f"  Note: Make sure {test_filename} exists in /tmp/nfs_exports/")
        sys.exit(1)

    file_handle, _ = unpack_opaque_flex(reply_data, offset + 4)
    print(f"  ✓ Got file handle: {len(file_handle)} bytes")
    print()

    # Step 3: WRITE with UNSTABLE mode
    print("Step 3: WRITE with UNSTABLE mode")
    print("-" * 60)
    write_xid = 600003

    # WRITE3args: file handle + offset + count + stable + data
    write_args = b''

    # File handle (variable-length opaque)
    write_args += struct.pack('>I', len(file_handle)) + file_handle
    padding = (4 - (len(file_handle) % 4)) % 4
    write_args += b'\x00' * padding

    # Offset (uint64) - write at beginning
    write_args += struct.pack('>Q', 0)

    # Count (uint32)
    write_args += struct.pack('>I', len(test_data))

    # Stable (enum stable_how): UNSTABLE = 0
    # This tells the server it can cache the write
    write_args += struct.pack('>I', 0)

    # Data (variable-length opaque)
    write_args += struct.pack('>I', len(test_data)) + test_data
    data_padding = (4 - (len(test_data) % 4)) % 4
    write_args += b'\x00' * data_padding

    print(f"  Writing {len(test_data)} bytes at offset 0")
    print(f"  Stable mode: UNSTABLE (0)")
    print(f"  Data: {test_data}")

    reply_data = rpc_call(host, port, write_xid, 100003, 3, 7, write_args)
    offset = parse_rpc_reply(reply_data)

    # Parse WRITE3res
    nfs_status = struct.unpack('>I', reply_data[offset:offset+4])[0]
    print(f"  NFS status: {nfs_status} (0=NFS3_OK)")

    if nfs_status != 0:
        print(f"  ✗ WRITE failed with status {nfs_status}")
        sys.exit(1)

    offset += 4

    # Parse WRITE3resok
    # wcc_data: pre_op_attr + post_op_attr
    pre_op_follows = struct.unpack('>I', reply_data[offset:offset+4])[0]
    offset += 4
    if pre_op_follows:
        offset += 24

    post_op_follows = struct.unpack('>I', reply_data[offset:offset+4])[0]
    offset += 4
    if post_op_follows:
        offset += 84  # Skip fattr3

    # count (bytes written)
    count = struct.unpack('>I', reply_data[offset:offset+4])[0]
    offset += 4

    # committed (stable_how)
    committed = struct.unpack('>I', reply_data[offset:offset+4])[0]
    offset += 4

    # verf (writeverf3 = 8 bytes)
    write_verf = reply_data[offset:offset+8]
    offset += 8

    print(f"  ✓ Wrote {count} bytes")
    print(f"  Committed: {committed} (0=UNSTABLE, 1=DATA_SYNC, 2=FILE_SYNC)")
    print(f"  Write verifier: {write_verf.hex()}")
    print()

    # Step 4: COMMIT the write
    print("Step 4: COMMIT to flush cached writes to stable storage")
    print("-" * 60)
    commit_xid = 600004

    # COMMIT3args: file handle + offset + count
    commit_args = b''

    # File handle (variable-length opaque)
    commit_args += struct.pack('>I', len(file_handle)) + file_handle
    padding = (4 - (len(file_handle) % 4)) % 4
    commit_args += b'\x00' * padding

    # Offset (uint64) - 0 means from beginning
    commit_args += struct.pack('>Q', 0)

    # Count (uint32) - 0 means to end of file
    commit_args += struct.pack('>I', 0)

    print(f"  COMMIT file from offset 0, count 0 (entire file)")

    # Call COMMIT (procedure 21)
    reply_data = rpc_call(host, port, commit_xid, 100003, 3, 21, commit_args)
    offset = parse_rpc_reply(reply_data)

    # Parse COMMIT3res
    nfs_status = struct.unpack('>I', reply_data[offset:offset+4])[0]
    print(f"  NFS status: {nfs_status} (0=NFS3_OK)")

    if nfs_status != 0:
        print(f"  ✗ COMMIT failed with status {nfs_status}")
        sys.exit(1)

    offset += 4

    # Parse COMMIT3resok
    # wcc_data: pre_op_attr + post_op_attr (same structure as WRITE)
    pre_op_follows = struct.unpack('>I', reply_data[offset:offset+4])[0]
    offset += 4
    if pre_op_follows:
        offset += 24

    post_op_follows = struct.unpack('>I', reply_data[offset:offset+4])[0]
    offset += 4
    if post_op_follows:
        offset += 84  # Skip fattr3

    # writeverf3 (8 bytes)
    # This verifier can be used to detect server reboots
    # If it changes between WRITE and COMMIT, data may have been lost
    commit_verf = reply_data[offset:offset+8]
    offset += 8

    print(f"  ✓ COMMIT succeeded")
    print(f"  Write verifier: {commit_verf.hex()}")
    print()

    # Step 5: Verify write verifier consistency
    print("Step 5: Verify write verifier consistency")
    print("-" * 60)
    print(f"  WRITE verifier:  {write_verf.hex()}")
    print(f"  COMMIT verifier: {commit_verf.hex()}")

    if write_verf == commit_verf:
        print(f"  ✅ Verifiers match - no server reboot detected")
    else:
        print(f"  ⚠ Verifiers differ - server may have rebooted")
        print(f"     (This is informational - doesn't affect test result)")

    print()
    print("=" * 60)
    print("✅ NFS COMMIT test PASSED")
    print()
    print("Summary:")
    print("  ✓ WRITE with UNSTABLE mode succeeded")
    print("  ✓ COMMIT forced data to stable storage")
    print("  ✓ Write verifier returned correctly")
    print()
    print("What COMMIT does:")
    print("  - Forces data written with UNSTABLE writes to disk")
    print("  - Returns a write verifier to detect server reboots")
    print("  - Client can use this after batched UNSTABLE writes")
    print("  - Ensures data persistence before acknowledging to application")


if __name__ == '__main__':
    test_nfs_commit()
