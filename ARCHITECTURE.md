# Arctic Wolf NFS Server - Architecture

## Overview

Arctic Wolf is an NFSv3 server implementation in Rust, providing NFSv3 and MOUNT protocol support with a clean, layered architecture inspired by [NFS-Ganesha](https://github.com/nfs-ganesha/nfs-ganesha).

## Design Principles

1. **Protocol Definition Separation**: XDR protocol specifications are centralized and separate from implementation
2. **Layered Architecture**: Clear separation between protocol, middleware, and business logic
3. **Type Safety**: Leverage Rust's type system with XDR-generated types
4. **One Operation Per File**: Each protocol operation in its own module for maintainability
5. **Version Isolation**: Different protocol versions are cleanly separated

## Directory Structure

```
arcticwolf/
├── xdr/                        # XDR Protocol Specifications
│   └── v3/                     # NFSv3 and related protocols
│       ├── rpc.x               # RPC protocol (RFC 5531)
│       ├── portmap.x           # PORTMAP protocol
│       ├── mount.x             # MOUNT protocol (RFC 1813)
│       └── nfs.x               # NFSv3 protocol (RFC 1813)
│
├── src/
│   ├── protocol/               # Protocol Middleware Layer
│   │   ├── mod.rs
│   │   └── v3/
│   │       ├── mod.rs
│   │       ├── rpc.rs          # RPC type wrappers + helpers
│   │       ├── portmap.rs      # PORTMAP helpers
│   │       ├── mount.rs        # MOUNT type wrappers + helpers
│   │       └── nfs.rs          # NFS type wrappers + helpers
│   │
│   ├── rpc/                    # RPC Implementation Layer
│   │   ├── mod.rs
│   │   └── server.rs           # TCP server + record marking (RFC 5531)
│   │
│   ├── portmap/                # PORTMAP Protocol Handlers
│   │   ├── mod.rs
│   │   ├── dispatcher.rs       # Route PORTMAP procedures
│   │   ├── null.rs             # PORTMAP NULL
│   │   ├── getport.rs          # PORTMAP GETPORT
│   │   └── dump.rs             # PORTMAP DUMP
│   │
│   ├── mount/                  # MOUNT Protocol Handlers
│   │   ├── mod.rs
│   │   ├── dispatcher.rs       # Route MOUNT procedures
│   │   ├── null.rs             # MOUNT NULL procedure
│   │   ├── mnt.rs              # MOUNT MNT procedure
│   │   ├── umnt.rs             # MOUNT UMNT procedure
│   │   └── export.rs           # MOUNT EXPORT procedure
│   │
│   ├── nfs/                    # NFS Protocol Handlers
│   │   ├── mod.rs
│   │   ├── dispatcher.rs       # Route NFS procedures
│   │   ├── null.rs             # NFS NULL (proc 0)
│   │   ├── getattr.rs          # GETATTR (proc 1)
│   │   ├── setattr.rs          # SETATTR (proc 2)
│   │   ├── lookup.rs           # LOOKUP (proc 3)
│   │   ├── access.rs           # ACCESS (proc 4)
│   │   ├── read.rs             # READ (proc 6)
│   │   ├── write.rs            # WRITE (proc 7)
│   │   ├── create.rs           # CREATE (proc 8)
│   │   ├── readdir.rs          # READDIR (proc 16)
│   │   ├── fsstat.rs           # FSSTAT (proc 18)
│   │   ├── fsinfo.rs           # FSINFO (proc 19)
│   │   └── pathconf.rs         # PATHCONF (proc 20)
│   │
│   ├── fsal/                   # Filesystem Abstraction Layer
│   │   ├── mod.rs              # Filesystem trait, ExportRegistry trait, NfsBackend
│   │   ├── handle.rs           # FileHandle (with export uid prefix)
│   │   ├── local/              # Local filesystem backend
│   │   │   └── mod.rs
│   │   └── multi_export.rs     # MultiExportFilesystem: routes by handle uid prefix
│   │
│   └── main.rs                 # Server entry point
│
├── tests/                      # Integration tests
│   ├── test_rpc_null.py        # RPC NULL test
│   ├── test_portmap.py         # PORTMAP tests
│   ├── test_mount_null.py      # MOUNT NULL test
│   ├── test_mount.py           # MOUNT protocol tests
│   ├── test_nfs_null.py        # NFS NULL test
│   ├── test_nfs_getattr.py     # GETATTR tests
│   ├── test_nfs_lookup.py      # LOOKUP tests
│   ├── test_nfs_read.py        # READ tests
│   ├── test_nfs_write.py       # WRITE tests
│   ├── test_nfs_create.py      # CREATE tests
│   ├── test_nfs_setattr.py     # SETATTR tests
│   └── test_nfs_readdir.py     # READDIR tests
│
├── build.rs                    # XDR code generation (xdrgen)
├── Cargo.toml                  # Dependencies
├── Earthfile                   # Containerized build
├── TODO.md                     # Planned improvements
└── ARCHITECTURE.md             # This file
```

## Architecture Layers

### Layer 1: XDR Protocol Specifications (`xdr/`)

**Purpose**: RFC-compliant protocol definitions in XDR (External Data Representation) format.

**Characteristics**:
- Pure data structure definitions
- Version-specific (v3/, v4/)
- Machine-readable, generates Rust code
- Single source of truth for protocol types

**Critical Design Note - XDR Unions**:
XDR supports two distinct data structures that are often confused:

1. **Struct** - Always serializes all fields:
```xdr
struct set_mode3 {
    bool set_it;
    uint32 mode;
};
// Always 8 bytes: set_it (4) + mode (4)
```

2. **Union** - Only serializes discriminator + value when needed:
```xdr
union set_mode3 switch (set_mode3_how set_it) {
    case SET_MODE:
        uint32 mode;
    default:
        void;
};
// 4 bytes when DONT_SET, 8 bytes when SET_MODE
```

**Our Implementation**: We use **unions** for optional attributes (set_mode3, set_uid3, set_gid3, set_size3) to match Linux NFS client behavior and RFC 1813 semantics. This was a critical fix - initial struct implementation caused "failed to fill whole buffer" errors with real NFS clients.

**Example** (`xdr/v3/nfs.x`):
```xdr
enum set_mode3_how {
    DONT_SET_MODE = 0,
    SET_MODE = 1
};

union set_mode3 switch (set_mode3_how set_it) {
    case SET_MODE:
        uint32 mode;
    default:
        void;
};

struct sattr3 {
    set_mode3 mode;
    set_uid3 uid;
    set_gid3 gid;
    set_size3 size;
    set_atime atime;
    set_mtime mtime;
};
```

**Build Process**:
```
xdr/v3/*.x → (xdrgen) → target/.../[protocol]_generated.rs
```

### Layer 2: Protocol Middleware (`src/protocol/`)

**Purpose**: Wraps generated XDR types with Rust-friendly interfaces and serialization helpers.

**Responsibilities**:
- Include XDR-generated types
- Provide serialization/deserialization methods
- Offer convenience constructors for responses
- Re-export types for use by upper layers

**Example** (`src/protocol/v3/nfs.rs`):
```rust
pub struct NfsMessage;

impl NfsMessage {
    // Deserialize NFS GETATTR args
    pub fn deserialize_getattr3args(data: &[u8]) -> Result<GETATTR3args> {
        let (args, _) = GETATTR3args::unpack(&mut Cursor::new(data))?;
        Ok(args)
    }

    // Create GETATTR success response
    pub fn create_getattr_response(attrs: &fattr3) -> Result<BytesMut> {
        let res = GETATTR3res::NFS3_OK(GETATTR3resok {
            obj_attributes: *attrs,
        });
        let mut buf = Vec::new();
        res.pack(&mut buf)?;
        Ok(BytesMut::from(&buf[..]))
    }

    // Convert FSAL attributes to NFS format
    pub fn fsal_to_fattr3(attrs: &FileAttributes) -> fattr3 {
        // Conversion logic
    }
}
```

**Key Features**:
- Type-safe wrappers around generated XDR code
- Automatic serialization via `xdr-codec` traits (`Pack`/`Unpack`)
- Convenience methods for common operations
- FSAL ↔ NFS attribute conversions

### Layer 3: RPC Implementation (`src/rpc/`)

**Purpose**: TCP server handling and RPC record marking protocol.

**Responsibilities**:
- Accept TCP connections on port 4000
- Handle RPC record marking (RFC 5531 §11)
- Parse RPC messages
- Route to protocol dispatchers (PORTMAP, MOUNT, NFS)
- Send formatted responses

**Record Marking Protocol**:
```
[last:1bit][length:31bits][fragment_data]
```

**Example** (`src/rpc/server.rs`):
```rust
async fn handle_connection(mut socket: TcpStream) -> Result<()> {
    loop {
        // Read record marking header (4 bytes)
        let header_u32 = socket.read_u32().await?;
        let is_last = (header_u32 & 0x80000000) != 0;
        let fragment_len = (header_u32 & 0x7FFFFFFF) as usize;

        // Read fragment
        let mut fragment = vec![0u8; fragment_len];
        socket.read_exact(&mut fragment).await?;

        // Assemble complete message
        message.extend_from_slice(&fragment);

        if is_last {
            // Process complete RPC message
            let reply = dispatch_rpc_message(&message).await?;

            // Send response with record marking
            let reply_len = reply.len() as u32;
            let header = 0x80000000 | reply_len;
            socket.write_u32(header).await?;
            socket.write_all(&reply).await?;

            message.clear();
        }
    }
}
```

### Layer 4: Protocol Dispatchers

**Purpose**: Route RPC calls to appropriate protocol handlers based on program and procedure numbers.

**Dispatchers**:
- `portmap::dispatcher` - Program 100000
- `mount::dispatcher` - Program 100005
- `nfs::dispatcher` - Program 100003

**Example** (`src/nfs/dispatcher.rs`):
```rust
pub async fn dispatch(
    call: &rpc_call_msg,
    args_data: &[u8],
    backend: &dyn NfsBackend,
) -> Result<BytesMut> {
    let xid = call.xid;
    match call.proc_ {
        0 => null::handle_null(xid).await,
        1 => getattr::handle_getattr(xid, args_data, backend).await,
        2 => setattr::handle_setattr(xid, args_data, backend).await,
        3 => lookup::handle_lookup(xid, args_data, backend).await,
        // ... 4–17 elided ...
        18 => fsstat::handle_fsstat(xid, args_data, backend).await,
        19 => fsinfo::handle_fsinfo(xid, args_data, backend).await,
        20 => pathconf::handle_pathconf(xid, args_data, backend).await,
        21 => commit::handle_commit(xid, args_data, backend).await,
        _ => create_notsupp_response(xid),
    }
}
```

Dispatchers now take `&dyn NfsBackend` (a trait combining `Filesystem +
ExportRegistry`). MOUNT receives the `ExportRegistry` view to resolve
`dirpath → root handle`; NFS handlers get both views from the same `Arc`
so write-class handlers can call `check_writable(backend, handle)` before
mutating an export.

### Layer 5: Protocol Handlers (`src/portmap/`, `src/mount/`, `src/nfs/`)

**Purpose**: Business logic for individual protocol operations.

**Design**: One operation per file (following nfs-ganesha pattern).

**Example** (`src/nfs/getattr.rs`):
```rust
pub async fn handle_getattr(
    xid: u32,
    args_data: &[u8],
    backend: &dyn NfsBackend,
) -> Result<BytesMut> {
    // 1. Deserialize arguments
    let args = NfsMessage::deserialize_getattr3args(args_data)?;

    // 2. Call FSAL to get attributes (routes by handle uid prefix)
    let attrs = backend.getattr(&args.object.0).await?;

    // 3. Convert FSAL attributes to NFS format
    let nfs_attrs = NfsMessage::fsal_to_fattr3(&attrs);

    // 4. Create and serialize response
    let res_data = NfsMessage::create_getattr_response(&nfs_attrs)?;

    // 5. Wrap in RPC reply
    RpcMessage::create_success_reply_with_data(xid, res_data)
}
```

**Handler Patterns**:
- Deserialize args using protocol middleware
- Validate inputs
- Call FSAL operations
- Convert results to protocol types
- Create response message
- Handle errors with appropriate NFS error codes

### Layer 6: Filesystem Abstraction (`src/fsal/`)

**Purpose**: Abstract different filesystem backends behind a common interface.

**FSAL Trait** (abbreviated — see `src/fsal/mod.rs` for full docs):
```rust
#[async_trait]
pub trait Filesystem: Send + Sync {
    // Metadata
    async fn getattr(&self, handle: &FileHandle) -> Result<FileAttributes>;
    async fn setattr_size(&self, handle: &FileHandle, size: u64) -> Result<()>;
    async fn setattr_mode(&self, handle: &FileHandle, mode: u32) -> Result<()>;
    async fn setattr_owner(&self, handle: &FileHandle, uid: Option<u32>, gid: Option<u32>)
        -> Result<()>;

    // Lookup and navigation
    async fn lookup(&self, dir_handle: &FileHandle, name: &str) -> Result<FileHandle>;
    async fn readlink(&self, handle: &FileHandle) -> Result<String>;

    // Data
    async fn read(&self, handle: &FileHandle, offset: u64, count: u32) -> Result<Vec<u8>>;
    async fn write(&self, handle: &FileHandle, offset: u64, data: &[u8]) -> Result<u32>;
    async fn commit(&self, handle: &FileHandle, offset: u64, count: u32) -> Result<()>;

    // Directory listing
    async fn readdir(&self, dir_handle: &FileHandle, cookie: u64, count: u32)
        -> Result<(Vec<DirEntry>, bool)>;

    // Namespace mutation
    async fn create(&self, dir_handle: &FileHandle, name: &str, mode: u32) -> Result<FileHandle>;
    async fn remove(&self, dir_handle: &FileHandle, name: &str) -> Result<()>;
    async fn mkdir(&self, dir_handle: &FileHandle, name: &str, mode: u32) -> Result<FileHandle>;
    async fn rmdir(&self, dir_handle: &FileHandle, name: &str) -> Result<()>;
    async fn rename(&self, from_dir: &FileHandle, from_name: &str,
                    to_dir: &FileHandle, to_name: &str) -> Result<()>;
    async fn symlink(&self, dir_handle: &FileHandle, name: &str, target: &str)
        -> Result<FileHandle>;
    async fn link(&self, file_handle: &FileHandle, dir_handle: &FileHandle, name: &str)
        -> Result<FileHandle>;
    async fn mknod(&self, dir_handle: &FileHandle, name: &str,
                   file_type: FileType, mode: u32, rdev: (u32, u32)) -> Result<FileHandle>;

    // Per-export filesystem stats — backs FSSTAT/FSINFO/PATHCONF
    // (the static, server-wide fields stay in the handlers)
    async fn fs_stats(&self, handle: &FileHandle) -> Result<FsStats>;
}
```

Note: `fsstat` / `fsinfo` / `pathconf` are **NFS handlers**, not FSAL trait
methods. The trait exposes a single `fs_stats(handle)` that returns the
per-export bits (free space, name-max, link-max, …); the handlers combine
that with server-wide constants (`rtmax`, `wtmax`, etc.) to assemble the
NFSv3 replies.

Per-export root handles also do not live on `Filesystem` — a multi-export
backend has no single "the" root. MOUNT obtains them via the separate
`ExportRegistry` trait below.

**ExportRegistry Trait**:
```rust
pub trait ExportRegistry: Send + Sync {
    /// Resolve the dirpath advertised to clients to that export's root handle.
    fn root_handle_for(&self, name: &str) -> Option<FileHandle>;

    /// Enumerate every configured export (used by MOUNT EXPORT + banners).
    fn list_exports(&self) -> Vec<ExportInfo>;

    /// True if the export that owns `handle` is read-only.
    fn is_read_only(&self, handle: &FileHandle) -> bool;

    /// Decode the export uid embedded in `handle`'s prefix.
    fn export_for_handle(&self, handle: &FileHandle) -> Option<u32>;
}

/// Combined trait used by `RpcServer`: one `Arc<dyn NfsBackend>` hands
/// MOUNT the `ExportRegistry` view and NFS the `Filesystem` view.
pub trait NfsBackend: Filesystem + ExportRegistry {}
impl<T: Filesystem + ExportRegistry> NfsBackend for T {}
```

**Current Implementation**:
- `local/mod.rs`: Local filesystem backend (one instance per export, std::fs)
- `multi_export.rs`: `MultiExportFilesystem` wraps a set of backends and
  dispatches `Filesystem` calls by the uid prefix in each `FileHandle`;
  it also implements `ExportRegistry` for the MOUNT path

**Future Backends**:
- `memory.rs`: In-memory filesystem (testing)
- `s3.rs`: S3-backed filesystem
- Custom backends for specific use cases

## Data Flow

### Complete NFS Operation Flow

```
Linux NFS Client
  ↓ TCP (port 4000)
[RPC Server: src/rpc/server.rs]
  ↓ Read record marking
  ↓ Parse RPC header (program, version, procedure)
  ↓ Route by program number
  ├─ 100000 → [PORTMAP Dispatcher]
  ├─ 100005 → [MOUNT Dispatcher]
  └─ 100003 → [NFS Dispatcher]
              ↓ Route by procedure
              ├─ proc=0 → null.rs
              ├─ proc=1 → getattr.rs
              ├─ proc=3 → lookup.rs
              ├─ proc=6 → read.rs
              └─ proc=7 → write.rs
                  ↓
              [Protocol Middleware: protocol::v3::nfs]
                  ↓ Deserialize WRITE3args
                  ↓ { file: fhandle3, offset, count, data }
              [FSAL: Filesystem trait]
                  ↓ write(handle, offset, data)
              [Local Backend: fsal::local]
                  ↓ std::fs operations
              [Response Path]
                  ↓ FSAL returns bytes_written
                  ↓ Handler creates WRITE3res
                  ↓ Middleware serializes response
                  ↓ RPC server adds record marking
                  ↓ Send to client
Linux NFS Client
```

### NFS WRITE Example (Detailed)

1. **Client Request**:
   - Client: `echo "test" > /mnt/nfs/file.txt`
   - Generates: SETATTR (truncate) + WRITE calls

2. **SETATTR Call**:
   ```
   RPC Header: prog=100003, vers=3, proc=2
   SETATTR3args:
     file: fhandle3 (32 bytes)
     new_attributes: sattr3 {
       mode: DONT_SET_MODE
       uid: DONT_SET_UID
       gid: DONT_SET_GID
       size: SET_SIZE(0)  // Truncate to 0
       atime: DONT_CHANGE
       mtime: DONT_CHANGE
     }
   ```

3. **Server Processing**:
   ```
   nfs::dispatcher → setattr::handle_setattr
     → NfsMessage::deserialize_setattr3args
     → filesystem.setattr_size(handle, 0)
     → LocalFilesystem uses std::fs::File::set_len(0)
     → Return SETATTR3res::NFS3_OK
   ```

4. **WRITE Call**:
   ```
   RPC Header: prog=100003, vers=3, proc=7
   WRITE3args:
     file: fhandle3
     offset: 0
     count: 5
     stable: FILE_SYNC
     data: b"test\n"
   ```

5. **Server Processing**:
   ```
   nfs::dispatcher → write::handle_write
     → NfsMessage::deserialize_write3args
     → filesystem.write(handle, 0, b"test\n")
     → LocalFilesystem uses std::fs::File::write_at
     → Return WRITE3res::NFS3_OK { count: 5, committed: FILE_SYNC }
   ```

## Technology Stack

### Core Dependencies

| Component | Library | Version | Purpose |
|-----------|---------|---------|---------|
| **Async Runtime** | `tokio` | 1.x | Async TCP server, concurrency |
| **XDR Codegen** | `xdrgen` | 0.4 | Generate Rust types from .x files |
| **XDR Runtime** | `xdr-codec` | 0.4 | Pack/Unpack traits for serialization |
| **Error Handling** | `anyhow` | 1.0 | Ergonomic error propagation |
| **Logging** | `tracing` | 0.1 | Structured logging |
| **Bytes** | `bytes` | 1.5 | Efficient byte buffer management |

### Build Tools

- **xdrgen**: CLI tool for XDR→Rust code generation (installed via `cargo install xdrgen`)
- **build.rs**: Runs xdrgen during compilation, applies Copy trait fixes
- **Earthly**: Containerized builds for reproducibility

## Protocol Support

### Current Implementation Status

| Protocol | Version | RFC | Port | Status |
|----------|---------|-----|------|--------|
| RPC | 2 | RFC 5531 | - | ✅ Complete |
| PORTMAP | 2 | RFC 1833 | - | ✅ Complete |
| MOUNT | 3 | RFC 1813 | - | ✅ Complete |
| NFS | 3 | RFC 1813 | 4000 | ✅ 22/22 procedures |

### Procedure Implementation

#### RPC (Program 100000) - Internal
- ✅ NULL (0) - Ping test

#### PORTMAP (Program 100000) - Port 4000
- ✅ NULL (0) - Ping test
- ✅ GETPORT (3) - Get port for program/version
- ✅ DUMP (4) - List all registered services

#### MOUNT (Program 100005) - Port 4000
- ✅ NULL (0) - Ping test
- ✅ MNT (1) - Mount directory, return file handle
- ✅ DUMP (2) - List all mounts
- ✅ UMNT (3) - Unmount directory
- ✅ UMNTALL (4) - Unmount all
- ✅ EXPORT (5) - List exported directories

#### NFSv3 (Program 100003) - Port 4000

**All Procedures Implemented** (22/22):
- ✅ NULL (0) - Ping test
- ✅ GETATTR (1) - Get file attributes
- ✅ SETATTR (2) - Set file attributes (size, mode, owner)
- ✅ LOOKUP (3) - Look up filename
- ✅ ACCESS (4) - Check access permissions
- ✅ READLINK (5) - Read symbolic link
- ✅ READ (6) - Read from file
- ✅ WRITE (7) - Write to file
- ✅ CREATE (8) - Create file
- ✅ MKDIR (9) - Create directory
- ✅ SYMLINK (10) - Create symbolic link
- ✅ MKNOD (11) - Create special file (FIFO, device)
- ✅ REMOVE (12) - Remove file
- ✅ RMDIR (13) - Remove directory
- ✅ RENAME (14) - Rename file/directory
- ✅ LINK (15) - Create hard link
- ✅ READDIR (16) - Read directory entries
- ✅ READDIRPLUS (17) - Extended READDIR with attributes
- ✅ FSSTAT (18) - Get filesystem statistics
- ✅ FSINFO (19) - Get filesystem static info
- ✅ PATHCONF (20) - Get POSIX path info
- ✅ COMMIT (21) - Commit cached data to stable storage

## Testing Implications

This issue revealed that **Python integration tests were insufficient**:
- Tests sent incorrect 60-byte format
- Server's lenient deserializer only read needed bytes, ignored extra
- Tests passed, but real Linux clients failed
- **Lesson**: Need strict format validation in tests (see TODO.md)

## Design Decisions

### Why Centralize XDR Files in `xdr/`?

**Inspired by**: NFS-Ganesha's `src/Protocols/XDR/` directory

**Benefits**:
1. **Single Source of Truth**: All protocol definitions in one place
2. **Clear Separation**: Protocol specs vs. implementation
3. **Version Management**: Easy to see v3 vs. v4 differences
4. **Build Simplification**: One place to run code generation
5. **Future Extensibility**: Can support multiple generators

### Why One Operation Per File?

**Inspired by**: NFS-Ganesha's pattern (`nfs3_getattr.c`, `nfs4_op_read.c`)

**Benefits**:
1. **Easy Navigation**: `git grep "GETATTR"` → one file
2. **Clear Git History**: Changes to READ don't pollute WRITE history
3. **Parallel Development**: Different developers work on different operations
4. **Focused Testing**: Unit tests per operation
5. **Code Review**: Smaller, focused diffs

### Why Protocol Middleware Layer?

**Purpose**: Bridge between raw XDR types and business logic

**Without Middleware**:
```rust
// Handler directly uses generated types - messy!
use target::..::nfs_generated::*;  // ❌ Ugly path
let (msg, _) = GETATTR3args::unpack(&mut cursor)?;  // ❌ Repeated everywhere
```

**With Middleware**:
```rust
use crate::protocol::v3::NfsMessage;  // ✅ Clean import
let args = NfsMessage::deserialize_getattr3args(data)?;  // ✅ Ergonomic API
```

**Additional Benefits**:
- Type conversions (e.g., FSAL ↔ NFS attributes)
- Validation before reaching handlers
- Logging/tracing instrumentation
- Metrics collection points

### Why Filesystem Abstraction Layer (FSAL)?

**Purpose**: Support multiple storage backends without changing NFS code

**Benefits**:
1. **Pluggable Backends**: Local fs, S3, memory, custom
2. **Testing**: Mock filesystem for unit tests
3. **Portability**: Same NFS code works with any backend
4. **Future Features**: Caching, replication, virtual filesystems

## Testing

### Integration Tests

All tests are Python scripts in `tests/`:

| Test File | Purpose | Status |
|-----------|---------|--------|
| `test_rpc_null.py` | RPC NULL procedure | ✅ |
| `test_portmap.py` | PORTMAP procedures | ✅ |
| `test_mount_null.py` | MOUNT NULL | ✅ |
| `test_mount.py` | MOUNT MNT/UMNT/EXPORT | ✅ |
| `test_nfs_null.py` | NFS NULL | ✅ |
| `test_nfs_getattr.py` | NFS GETATTR | ✅ |
| `test_nfs_lookup.py` | NFS LOOKUP | ✅ |
| `test_nfs_read.py` | NFS READ | ✅ |
| `test_nfs_write.py` | NFS WRITE | ✅ |
| `test_nfs_create.py` | NFS CREATE | ✅ |
| `test_nfs_setattr.py` | NFS SETATTR (truncate) | ✅ |
| `test_nfs_readdir.py` | NFS READDIR | ✅ |

### Running Tests

```bash
# Start server
cargo run

# Run all tests (separate terminal)
for test in tests/test_*.py; do
    echo "Running $test..."
    python3 "$test"
done
```

### Real-World Testing

```bash
# Mount as NFS client
sudo mount -t nfs -o vers=3,proto=tcp,port=4000,mountport=4000,nolock,noresvport,nordirplus localhost:/ /mnt/test

# Test operations
echo "Hello NFS" > /mnt/test/file.txt
cat /mnt/test/file.txt
ls -la /mnt/test/
```

### Future Testing Strategy

1. **Unit Tests**: Per-operation handlers with mock FSAL
2. **Integration Tests**: Full RPC→FSAL→Response flow
3. **Compliance Tests**: RFC 1813 test suite
4. **Performance Tests**: Throughput, latency benchmarks
5. **Compatibility Tests**: Multiple NFS clients (Linux, macOS, Windows)
6. **Stress Tests**: Concurrent operations, large files
7. **Format Validation**: Strict XDR byte-level tests (see TODO.md)

## Build & Development

### Prerequisites

```bash
# Install xdrgen (one-time)
cargo install xdrgen

# Install Rust (if needed)
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
```

### Local Development

```bash
# Build
cargo build

# Run with logging
RUST_LOG=debug cargo run

# Run tests
cargo test

# Format code
cargo fmt

# Lint
cargo clippy
```

### Docker Build (Earthly)

```bash
# Full build
make build

# Output: build/release/arcticwolf
./build/release/arcticwolf
```

### Adding a New NFS Operation

1. **Update XDR** (if needed): `xdr/v3/nfs.x`
2. **Add Handler**: `src/nfs/operation_name.rs`
3. **Register in Dispatcher**: `src/nfs/dispatcher.rs`
4. **Add FSAL Method** (if needed): `src/fsal/mod.rs`
5. **Implement in Local Backend**: `src/fsal/local.rs`
6. **Add Integration Test**: `tests/test_nfs_operation.py`
7. **Update Documentation**: This file

## Known Issues & Lessons Learned

### 1. XDR Union vs Struct Confusion
**Issue**: Used struct for optional attributes, caused "failed to fill whole buffer" errors with Linux clients.

**Solution**: Changed to union types. See [XDR Union vs Struct](#xdr-union-vs-struct---critical-lesson) section.

**Lesson**: XDR unions are not just enums - they control serialization. Always use unions for optional fields in NFS protocols.

### 2. Python Tests Not Catching Format Errors
**Issue**: Integration tests passed even with incorrect XDR format because server's deserializer is lenient.

**Solution**: Need to add strict byte-level validation (see Python Test Improvements below).

**Lesson**: Integration tests need to verify exact wire format, not just end-to-end functionality.

### 3. Copy Trait for Union Types
**Issue**: xdrgen generates `Copy` trait for union types containing `Box<T>`, causing compile errors.

**Solution**: `build.rs` post-processes generated code to remove `Copy` from affected types.

**Lesson**: Code generation always needs some manual fixups for edge cases.

## Implementation Status

All 22 NFSv3 procedures are implemented — see the
[Procedure Implementation](#procedure-implementation) table above for the
authoritative per-procedure status. The remaining work tracked here is
testing, security, and production-readiness rather than new procedures.

### Python Test Improvements Needed

The current Python integration tests (`tests/test_nfs_*.py`) need the following improvements:

1. **Add Length Assertions for XDR Union Format**
   - Currently tests only check functional correctness
   - Need to verify exact byte lengths of serialized data
   - Example: `sattr3` with all DONT_SET should be exactly 24 bytes (6 discriminators × 4 bytes)
   - Example: `sattr3` with SET_SIZE should be 32 bytes (5 discriminators + 1 discriminator + u64 size)

2. **Add Strict Format Validation Tests**
   - Create tests that specifically detect struct vs union mismatches
   - Send deliberately wrong formats and verify server rejects them
   - Test boundary cases (partial data, extra data, wrong discriminator values)

3. **Consider Adding tcpdump-based Tests**
   - Capture real Linux NFS client traffic with tcpdump
   - Compare our server's responses with reference implementations
   - Validate wire format matches RFC 1813 exactly

**Files to improve:**
- `tests/test_nfs_create.py` - Add sattr3 length validation
- `tests/test_nfs_setattr.py` - Add sattr3 length validation
- `tests/test_nfs_write.py` - Add general response format validation

### Other Improvements

**Code Quality:**
- Add comprehensive error handling for all edge cases
- Improve logging and debugging output
- Add performance benchmarks

**Security:**
- Implement AUTH_UNIX authentication (currently using AUTH_NONE)
- Add proper access control checks
- Validate file handle security

**Configuration:**
- Add runtime configuration reload (TOML config + multi-export are already
  in place — see `src/config.rs` and `src/fsal/multi_export.rs`)

**Production Readiness:**
- Add metrics and monitoring (Prometheus, etc.)
- Implement proper daemon mode
- Add systemd service file
- Create comprehensive documentation

## References

### RFCs
- [RFC 5531](https://www.rfc-editor.org/rfc/rfc5531): RPC Protocol Specification Version 2
- [RFC 4506](https://www.rfc-editor.org/rfc/rfc4506): XDR: External Data Representation Standard
- [RFC 1813](https://www.rfc-editor.org/rfc/rfc1813): NFS Version 3 Protocol Specification
- [RFC 1833](https://www.rfc-editor.org/rfc/rfc1833): Binding Protocols for ONC RPC Version 2
- [RFC 7530](https://www.rfc-editor.org/rfc/rfc7530): NFSv4 Protocol

### Inspirations
- [NFS-Ganesha](https://github.com/nfs-ganesha/nfs-ganesha): Production NFS server in C
- [zerofs_nfsserve](https://github.com/Barre/zerofs_nfsserve): Reference implementation that helped solve the union issue
- [nfs-server-rs](https://github.com/xetdata/nfs-server-rs): Rust NFS implementation
- [rust-xdr](https://github.com/jsgf/rust-xdr): xdrgen and xdr-codec libraries

### Crates
- [xdrgen](https://crates.io/crates/xdrgen): XDR code generator
- [xdr-codec](https://crates.io/crates/xdr-codec): XDR serialization runtime
- [tokio](https://tokio.rs/): Async runtime
- [anyhow](https://crates.io/crates/anyhow): Error handling
- [tracing](https://crates.io/crates/tracing): Structured logging

---

**Last Updated**: 2026-05-11 (refreshed for multi-export feature, #26)
