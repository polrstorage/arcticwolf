# Earthfile

VERSION 0.8


common:
    FROM rust:1.91
    WORKDIR /src

    # Install xdrgen for XDR code generation
    RUN cargo install xdrgen

    # Copy Cargo files (Cargo.lock is optional, Earthly will ignore if missing)
    COPY Cargo.toml Cargo.lock* ./

    # Copy build script for XDR type generation
    COPY build.rs ./

    # Copy XDR protocol specifications
    COPY xdr ./xdr

    # Copy source code
    COPY src ./src

    # Pre-fetch dependencies to speed up subsequent builds
    RUN cargo fetch

# earthly +build
build:
    FROM +common
    RUN cargo build --release
    SAVE ARTIFACT target/release/arcticwolf AS LOCAL build/release/arcticwolf

# earthly +test
test:
    FROM +common
    RUN cargo test

# earthly +lint
lint:
    FROM +common
    RUN rustup component add clippy rustfmt
    RUN cargo fmt -- --check
    RUN cargo clippy -- -D warnings

# earthly +server-docker
# Build Docker image using the same build environment
server-docker:
    ARG IMAGE_NAME=arcticwolf
    ARG IMAGE_TAG=latest
    FROM +common
    RUN cargo build
    RUN mkdir -p /tmp/nfs_exports
    ENV RUST_LOG=debug
    EXPOSE 4000
    ENTRYPOINT ["./target/debug/arcticwolf"]
    SAVE IMAGE ${IMAGE_NAME}:${IMAGE_TAG}

# earthly +client-vm
# Build Alpine VM image for integration testing
client-vm:
    ARG VM_OUTPUT_DIR=build/nfstest/vm
    ARG VM_IMAGE_NAME=vm.qcow2
    ARG CIDATA_NAME=cidata.iso
    FROM alpine:3.19
    RUN apk add --no-cache qemu-img cdrkit curl

    WORKDIR /build

    # Download Alpine nocloud image (x86_64 with cloud-init)
    RUN curl -L -o vm.qcow2 \
        https://dl-cdn.alpinelinux.org/alpine/v3.19/releases/cloud/nocloud_alpine-3.19.0-x86_64-bios-cloudinit-r0.qcow2

    # Resize image to have more space
    RUN qemu-img resize vm.qcow2 2G

    # Create cloud-init ISO (NoCloud datasource)
    RUN mkdir -p cidata
    COPY nfstest/vm/user-data ./cidata/user-data
    RUN echo "instance-id: nfstest-vm" > cidata/meta-data
    RUN genisoimage -output cidata.iso -volid cidata -joliet -rock cidata/

    # Export VM artifacts to local build directory
    SAVE ARTIFACT vm.qcow2 AS LOCAL ${VM_OUTPUT_DIR}/${VM_IMAGE_NAME}
    SAVE ARTIFACT cidata.iso AS LOCAL ${VM_OUTPUT_DIR}/${CIDATA_NAME}
