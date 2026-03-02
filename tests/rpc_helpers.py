#!/usr/bin/env python3
"""Shared RPC helper functions for NFS tests."""

import socket
import struct


def portmap_getport(host, prog, vers, prot=6):
    """Query portmapper to discover the port for a given RPC program.

    Args:
        host: Server hostname
        prog: RPC program number (e.g., 100005 for MOUNT)
        vers: Program version (e.g., 3)
        prot: Protocol (6=TCP, 17=UDP)

    Returns:
        Port number (int), or raises Exception if not found.
    """
    portmap_port = 111
    xid = 99990

    # Build RPC GETPORT call (program=100000, version=2, procedure=3)
    rpc_header = struct.pack(
        '>IIIII II II',
        xid,
        2,          # RPC version
        100000,     # Program (Portmapper)
        2,          # Version
        3,          # Procedure (GETPORT)
        0, 0,       # cred (AUTH_NONE)
        0, 0        # verf (AUTH_NONE)
    )

    # Mapping argument: prog, vers, prot, port (port ignored in query)
    mapping_arg = struct.pack('>IIII', prog, vers, prot, 0)

    call_msg = rpc_header + mapping_arg
    record_header = struct.pack('>I', 0x80000000 | len(call_msg))

    sock = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
    sock.settimeout(5.0)
    sock.connect((host, portmap_port))
    sock.sendall(record_header + call_msg)

    # Read response
    reply_header_bytes = sock.recv(4)
    if len(reply_header_bytes) != 4:
        sock.close()
        raise Exception("Failed to read portmapper response header")

    reply_len = struct.unpack('>I', reply_header_bytes)[0] & 0x7FFFFFFF
    reply_data = b''
    while len(reply_data) < reply_len:
        chunk = sock.recv(reply_len - len(reply_data))
        if not chunk:
            break
        reply_data += chunk
    sock.close()

    if len(reply_data) < 24:
        raise Exception(f"Portmapper response too short: {len(reply_data)} bytes")

    reply_xid, reply_stat, verf_flavor, verf_len, accept_stat = struct.unpack(
        '>IIIII', reply_data[:20]
    )

    if reply_stat != 0 or accept_stat != 0:
        raise Exception(f"Portmapper RPC error: reply_stat={reply_stat}, accept_stat={accept_stat}")

    result_port = struct.unpack('>I', reply_data[20:24])[0]
    if result_port == 0:
        raise Exception(f"Program {prog} v{vers} not registered with portmapper")

    return result_port
