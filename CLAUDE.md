# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Project Overview

Arctic Wolf is a Rust-based NFSv3 server implementing RFC 1813 (NFSv3), RFC 5531 (RPC), and RFC 1833 (PORTMAP). It provides a clean, layered architecture with a Filesystem Abstraction Layer (FSAL) for different storage backends.

## Build Commands

**Important:** Always use `make` commands for building and testing. Do not run `cargo` or `earthly` directly.

```bash
make build    # Build in container (Earthly)
make test     # Run unit tests (Earthly)
make lint     # Run clippy + rustfmt (Earthly)
make nfstest  # Full NFS integration test on Apple `container`
```

### Integration test (Apple `container`)

`make nfstest` runs the end-to-end NFSv3 test entirely on Apple's `container`
runtime — no Docker, Earthly, or QEMU. It boots the Arctic Wolf server and an
`nfstest_posix` client as two containers on the default `192.168.64.0/24`
bridge; the client mounts the server's `/data` export and runs the upstream
POSIX read/write suite. Build inputs live under `nfstest/`:

- `nfstest/server/Dockerfile` — server image (`container build`).
- `nfstest/client/Dockerfile` — nfstest client image.
- `nfstest/kernel/` — a Linux kernel `Image` rebuilt from Apple's own kernel
  config with `CONFIG_NFS_FS=y` (the default Apple kernel has no NFS client).
  `make nfstest-kernel` builds it once and caches it in `nfstest/kernel/out/`.
- `nfstest/scripts/nfstest.py` — orchestration (build / start-server /
  run-test / stop) driven via the `container` CLI.

Override the POSIX test cases with `make nfstest TESTCASE=read,write`.

## Git Conventions

Every commit must be signed off (Developer Certificate of Origin). Always
create commits with `git commit -s` so a `Signed-off-by:` trailer is added.
When amending, use `git commit --amend -s`.

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

## Configuration Schema

Server configuration is loaded from `/etc/arcticwolf/config.toml` (or `--config <path>`).
The schema is enforced by three layers:

- `#[serde(deny_unknown_fields)]` on the top-level `Config` and on the tagged
  `BackendConfig` enum rejects unknown keys at the outer and backend layers. The
  `ExportConfig` struct itself cannot carry the attribute (it uses
  `#[serde(flatten)]` for `backend`), but typos at the export level still fail
  fast via cascade: serde routes any key not matched by `ExportConfig`'s direct
  fields through the flatten buffer into `BackendConfig`, whose
  `deny_unknown_fields` then errors out.
- `Config::load()` additionally wraps deserialization in `serde_ignored` to
  catch unknown keys in the non-flattened sections (`[server]`, `[logging]`)
  whose structs intentionally allow `#[serde(default)]`-style partial overrides
  and so cannot use `deny_unknown_fields`. `serde_ignored` cannot see flatten
  leftovers, so it complements rather than replaces the cascade above.
- `Config::validate()` then enforces invariants (uid != 0, unique uid, unique
  name, name starts with `/`, local backend path is absolute) before the server
  starts.

Any of these layers rejecting input causes a fail-fast startup error rather
than silent fallback.

### `[[exports]]`

Each NFS export is one entry in the `exports` array. At least one entry is required.

Required fields:

- `name` — export path advertised to clients (e.g. `"/data"`). Must start with `/` and
  be unique across all exports.
- `uid` — `u32` in the range `1..=u32::MAX` (i.e. non-zero). Must be unique across
  exports. The value is encoded into the first 4 bytes of every file handle this
  export hands out, so collisions would make handles ambiguous. `0` is reserved as an
  invalid sentinel and is rejected.
- `backend` — storage backend discriminator. Currently only `"local"` is supported.
  The field is deserialized as a tagged enum (`#[serde(tag = "backend")]`) so adding
  S3/Ceph/etc. later only requires a new enum variant — the TOML shape stays the same.
- backend-specific keys — sit alongside the keys above. For `backend = "local"`:
  - `path` — absolute path on the server's local filesystem.

Optional fields:

- `read_only` — if `true`, deny writes against this export. Defaults to `false`.

### Startup validation (fail-fast)

`Config::validate()` rejects:

- empty `exports` list
- any `uid == 0`
- duplicate `uid` across exports
- duplicate `name` across exports
- `name` not starting with `/`

### Removed: `[fsal]`

The single-export `[fsal]` section from earlier versions has been removed. Because the
top-level `Config` carries `#[serde(deny_unknown_fields)]`, a config file that still
contains `[fsal]` will fail to parse with an "unknown field" error rather than being
silently ignored.

### Minimal example

```toml
[server]
bind_address = "0.0.0.0"
nfs_port = 2049
mount_port = 0

[[exports]]
name = "/data"
uid = 1
backend = "local"
path = "/srv/nfs/data"
```

See `arcticwolf.example.toml` for a fuller annotated example.

## Code Quality Guidelines

### Logging Initialization Order
Do not use `tracing::info!` or other tracing macros before `tracing_subscriber::init()` - messages will be silently dropped. Use `println!` or `eprintln!` for output before tracing is initialized.

### Handle Invalid User Input Explicitly
Avoid silent fallbacks with `.parse().unwrap_or(default)`. Instead, warn users about invalid configuration:
```rust
let value = match input.parse() {
    Ok(v) => v,
    Err(_) => {
        eprintln!("Warning: Invalid value '{}', falling back to default", input);
        default
    }
};
```

### Avoid Redundant Output
Review startup messages to ensure each piece of information is shown only once. Don't print the same value in multiple places.

### Config Fields Must Be Used or Validated
If a config field is displayed but not used to control behavior, users will be misled. Either:
- Remove the field if not needed
- Validate and error on unsupported values
- Actually use the field to control behavior

