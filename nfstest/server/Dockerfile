# Arctic Wolf NFS server image, built with Apple `container build`.
#
# Replaces the Earthly `server-docker` target for the local integration
# test. Build from the repo root:
#
#   container build -t arcticwolf:test -f nfstest/server/Dockerfile .

FROM rust:1.91 AS build

# build.rs shells out to `xdrgen` to generate Rust types from xdr/v3/*.x.
RUN cargo install xdrgen

WORKDIR /src

# Cargo manifests first so dependency fetch caches independently of source.
COPY Cargo.toml Cargo.lock ./
COPY build.rs ./
COPY xdr ./xdr
COPY src ./src

RUN cargo build --release

FROM debian:bookworm-slim AS runtime

# Export roots must exist before startup: LocalFilesystem::new canonicalizes
# and stat()s each export path and fails fast if it is missing.
RUN mkdir -p /srv/nfs/data /srv/nfs/backup /etc/arcticwolf \
    && chmod 0777 /srv/nfs/data /srv/nfs/backup

COPY --from=build /src/target/release/arcticwolf /usr/local/bin/arcticwolf
COPY nfstest/config/arcticwolf.test.toml /etc/arcticwolf/config.toml

EXPOSE 111 2049

ENTRYPOINT ["/usr/local/bin/arcticwolf"]
