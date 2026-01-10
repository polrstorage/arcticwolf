# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Project Overview

Arctic Wolf is a Rust-based NFSv3 server implementing RFC 1813 (NFSv3), RFC 5531 (RPC), and RFC 1833 (PORTMAP). It provides a clean, layered architecture with a Filesystem Abstraction Layer (FSAL) for different storage backends.

## Build Commands

```bash
make build    # Build in container
make test     # Run unit tests
make lint     # Run clippy + rustfmt
make nfstest  # Full integration tests with VM
```

## Architecture

### Layer Structure

```
XDR Specifications (xdr/v3/*.x)
    ↓ (xdrgen generates Rust types via build.rs)
Protocol Middleware (src/protocol/v3/)
    ↓ (wraps XDR with serialization helpers)
RPC Server (src/rpc/server.rs)
    ↓ (TCP listener, RFC 5531 record marking)
Protocol Dispatchers (src/{portmap,mount,nfs}/dispatcher.rs)
    ↓ (routes by program/procedure number)
Protocol Handlers (individual operation files)
    ↓ (business logic per NFS operation)
FSAL (src/fsal/)
    ↓ (filesystem abstraction trait)
```

### Key Patterns

**One Operation Per File**: Each NFS procedure has its own module (e.g., `src/nfs/getattr.rs`, `src/nfs/read.rs`).

**Protocol Middleware**: `src/protocol/v3/nfs.rs` provides `NfsMessage` with serialization helpers and FSAL-to-XDR conversions.

**FSAL Trait**: `src/fsal/mod.rs` defines the `Filesystem` trait. `src/fsal/local.rs` implements the local filesystem backend.

### XDR Code Generation

`build.rs` runs `xdrgen` on `.x` files in `xdr/v3/` to generate Rust types. It post-processes output to remove `Copy` trait from union types containing `Box<T>`.

## Critical Design Note: XDR Unions vs Structs

NFS optional attributes use **unions** not structs:
- **Struct**: Always serializes all fields (fixed size)
- **Union**: Only serializes discriminator + active value (variable size)

Using structs for optional fields causes "failed to fill whole buffer" errors with real Linux NFS clients. See `xdr/v3/nfs.x` for correct patterns (e.g., `set_mode3`, `set_size3`).

## Adding a New NFS Operation

1. Update `xdr/v3/nfs.x` if new types needed
2. Create `src/nfs/operation_name.rs` with handler
3. Register in `src/nfs/dispatcher.rs` (match proc number)
4. Add FSAL method to `src/fsal/mod.rs` trait if needed
5. Implement in `src/fsal/local.rs`
6. Add test: `tests/test_nfs_operation.py`

