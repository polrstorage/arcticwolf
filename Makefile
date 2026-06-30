EARTHLY ?= earthly

# Docker image configuration (used by the Earthly `image` target only)
IMAGE_NAME ?= arcticwolf
IMAGE_REPO ?= freezevicente
IMAGE_TAG ?= latest

# Integration test orchestration (Apple `container`)
NFSTEST ?= ./nfstest/scripts/nfstest.py

# Test configuration
TESTCASE ?= read,write

# Default target
.DEFAULT_GOAL := help

.PHONY: help build test lint image lockfile \
        nfstest nfstest-build nfstest-kernel nfstest-up nfstest-down clean

# Show available targets and their descriptions
help:
	@echo "Available targets:"
	@echo "  build           - Build release binary (Earthly)"
	@echo "  test            - Run unit tests (Earthly)"
	@echo "  lint            - Run clippy and rustfmt checks (Earthly)"
	@echo "  lockfile        - Regenerate Cargo.lock from Cargo.toml"
	@echo "  nfstest         - Run NFS integration test on Apple containers"
	@echo "  nfstest-build   - Build server + client images"
	@echo "  nfstest-kernel  - Build the NFS-enabled client kernel"
	@echo "  nfstest-up      - Start the server container"
	@echo "  nfstest-down    - Stop and remove test containers"
	@echo "  clean           - Stop containers and remove build artifacts"
	@echo ""
	@echo "Examples:"
	@echo "  make nfstest                     # Run default tests (read,write)"
	@echo "  make nfstest TESTCASE=read       # Run only read test"
	@echo "  make nfstest TESTCASE=read,write # Run read and write tests"

# Build release binary
build:
	$(EARTHLY) +build

# Run unit tests
test:
	$(EARTHLY) +test

# Run clippy and rustfmt checks
lint:
	$(EARTHLY) +lint

# Regenerate Cargo.lock from Cargo.toml
lockfile:
	$(EARTHLY) +lockfile

# Build published Docker image (Earthly)
image:
	${EARTHLY} +image --IMAGE_REPO=$(IMAGE_REPO) --IMAGE_TAG=$(IMAGE_TAG)

# Build the NFS-enabled kernel for the client container (cached after first run)
nfstest-kernel:
	@$(NFSTEST) build-kernel

# Build server + client images
nfstest-build:
	@$(NFSTEST) build-images

# Start the server container
nfstest-up:
	@$(NFSTEST) start-server

# Stop and remove test containers
nfstest-down:
	@$(NFSTEST) stop

# Run the full integration test on Apple containers
nfstest: nfstest-kernel nfstest-build
	@$(NFSTEST) test --testcase=$(TESTCASE)

# Clean build artifacts and stop running test containers
clean: nfstest-down
	rm -rf target build
