EARTHLY ?= earthly

# Docker image configuration
IMAGE_NAME ?= arcticwolf
IMAGE_TAG ?= latest

# VM configuration
VM_OUTPUT_DIR ?= build/nfstest/vm
VM_IMAGE_NAME ?= vm.qcow2
CIDATA_NAME ?= cidata.iso

# Test configuration
TESTCASE ?= open,read,write

# Default target
.DEFAULT_GOAL := help

.PHONY: help build test lint start-test-env nfstest stop-test-env clean

# Show available targets and their descriptions
help:
	@echo "Available targets:"
	@echo "  build           - Build release binary"
	@echo "  test            - Run unit tests"
	@echo "  lint            - Run clippy and rustfmt checks"
	@echo "  start-test-env  - Build and start both server and client VM"
	@echo "  nfstest         - Run NFS tests (TESTCASE=open,read,write)"
	@echo "  stop-test-env   - Stop both server and VM"
	@echo "  clean           - Stop all and remove build artifacts"
	@echo ""
	@echo "Examples:"
	@echo "  make nfstest                    # Run default tests (open,read,write)"
	@echo "  make nfstest TESTCASE=open      # Run only open test"
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

# Build and start both server and VM
start-test-env:
	@$(EARTHLY) +server-docker --IMAGE_NAME=$(IMAGE_NAME) --IMAGE_TAG=$(IMAGE_TAG)
	@$(EARTHLY) +client-vm --VM_OUTPUT_DIR=$(VM_OUTPUT_DIR) --VM_IMAGE_NAME=$(VM_IMAGE_NAME) --CIDATA_NAME=$(CIDATA_NAME)
	@./nfstest/scripts/nfstest.py start-env --image-name=$(IMAGE_NAME) --image-tag=$(IMAGE_TAG) --vm-dir=$(VM_OUTPUT_DIR) --vm-image=$(VM_IMAGE_NAME) --cidata=$(CIDATA_NAME)

# Run NFS tests (builds if needed)
nfstest: stop-test-env start-test-env
	@./nfstest/scripts/nfstest.py test --image-name=$(IMAGE_NAME) --image-tag=$(IMAGE_TAG) --vm-dir=$(VM_OUTPUT_DIR) --vm-image=$(VM_IMAGE_NAME) --cidata=$(CIDATA_NAME) --testcase=$(TESTCASE)

# Stop both server and VM
stop-test-env:
	@./nfstest/scripts/nfstest.py stop-env

# Clean build artifacts and stop running test containers/VMs
clean: stop-test-env
	rm -rf target build
