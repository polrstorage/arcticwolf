#!/usr/bin/env python3
"""
Test RPC NULL call to the arcticwolf NFS server

This script sends a simple RPC NULL procedure call and expects a success response.
"""

import socket
import struct

def send_rpc_null_call(host='localhost', port=2049):
    """Send an RPC NULL call and verify the response"""

    # Build RPC call message
    # struct rpc_call_msg {
    #     unsigned int xid;
    #     unsigned int rpcvers;
    #     unsigned int prog;
    #     unsigned int vers;
    #     unsigned int proc;
    #     opaque_auth cred;
    #     opaque_auth verf;
    # };

    xid = 12345
    rpcvers = 2
    prog = 100003  # NFS program number
    vers = 3       # NFSv3
    proc = 0       # NULL procedure

    # Build the message
    message = b''
    message += struct.pack('!I', xid)      # XID
    message += struct.pack('!I', rpcvers)  # RPC version
    message += struct.pack('!I', prog)     # Program number
    message += struct.pack('!I', vers)     # Program version
    message += struct.pack('!I', proc)     # Procedure number

    # cred (AUTH_NONE)
    message += struct.pack('!I', 0)  # flavor = AUTH_NONE
    message += struct.pack('!I', 0)  # length = 0

    # verf (AUTH_NONE)
    message += struct.pack('!I', 0)  # flavor = AUTH_NONE
    message += struct.pack('!I', 0)  # length = 0

    print(f"Sending RPC NULL call to {host}:{port}")
    print(f"  XID: {xid}")
    print(f"  Program: {prog}, Version: {vers}, Procedure: {proc}")
    print(f"  Message size: {len(message)} bytes")
    print(f"  Message (hex): {message.hex()}")

    # Connect to server
    sock = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
    try:
        sock.connect((host, port))

        # Send with RPC record marking
        # Format: [last_fragment:1bit][length:31bits]
        fragment_header = 0x80000000 | len(message)  # last=true
        sock.send(struct.pack('!I', fragment_header))
        sock.send(message)

        print("\nWaiting for response...")

        # Read response fragment header
        response_header = sock.recv(4)
        if len(response_header) != 4:
            print(f"ERROR: Failed to read response header")
            return False

        resp_fragment = struct.unpack('!I', response_header)[0]
        resp_is_last = (resp_fragment & 0x80000000) != 0
        resp_length = resp_fragment & 0x7FFFFFFF

        print(f"Response fragment: last={resp_is_last}, length={resp_length}")

        # Read response body
        response = sock.recv(resp_length)
        if len(response) != resp_length:
            print(f"ERROR: Expected {resp_length} bytes, got {len(response)}")
            return False

        print(f"Response (hex): {response.hex()}")

        # Parse response
        # Format: [xid][reply_stat][verf][accept_stat]
        if len(response) < 16:
            print(f"ERROR: Response too short: {len(response)} bytes")
            return False

        resp_xid = struct.unpack('!I', response[0:4])[0]
        reply_stat = struct.unpack('!I', response[4:8])[0]
        verf_flavor = struct.unpack('!I', response[8:12])[0]
        verf_len = struct.unpack('!I', response[12:16])[0]

        print(f"\nParsed response:")
        print(f"  XID: {resp_xid} (expected {xid})")
        print(f"  Reply stat: {reply_stat} (0=MSG_ACCEPTED)")
        print(f"  Verf flavor: {verf_flavor} (0=AUTH_NONE)")
        print(f"  Verf length: {verf_len}")

        if verf_len == 0 and len(response) >= 20:
            accept_stat = struct.unpack('!I', response[16:20])[0]
            print(f"  Accept stat: {accept_stat} (0=SUCCESS)")

            if resp_xid == xid and reply_stat == 0 and accept_stat == 0:
                print("\n✅ RPC NULL call succeeded!")
                return True
            else:
                print("\n❌ Unexpected response values")
                return False
        else:
            print("\n❌ Failed to parse accept_stat")
            return False

    except Exception as e:
        print(f"\n❌ Error: {e}")
        return False
    finally:
        sock.close()

if __name__ == '__main__':
    import sys
    host = sys.argv[1] if len(sys.argv) > 1 else 'localhost'
    port = int(sys.argv[2]) if len(sys.argv) > 2 else 2049

    success = send_rpc_null_call(host, port)
    sys.exit(0 if success else 1)
