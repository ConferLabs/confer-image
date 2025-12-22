# Confer Image

Confidential VM image for running Confer in TEE environments.

## Overview

This repository uses nix and mkosi to build a minimal and fully reproducible Linux image designed to run inside a Trusted Execution Environment (TEE). The image includes:

- Ubuntu Noble (24.04) base
- NVIDIA drivers
- vLLM for LLM inference
- Java runtime for the Confer proxy
- dm-verity for runtime integrity verification

## Architecture

The image uses dm-verity to cryptographically bind the entire root filesystem to the attestation:

1. The root filesystem hash is embedded in the kernel command line
2. The kernel command line is measured into the TEE during boot
3. Clients verify these measurements match a signed manifest in a transparency log before trusting the enclave
4. All builds from the transparency log can be reproduced by third parties to verify that they match the signed measurements

Any modification to the disk image will change the attestation measurements.

## Building

Prerequisites:
- Nix with flakes enabled

```bash
# Enter the development environment
nix develop

# Build the image
make build
```

Output files:
- `confer-image_<version>.vmlinuz` - Linux kernel
- `confer-image_<version>.initrd` - Initial ramdisk
- `confer-image_<version>.qcow2` - Root filesystem
- `confer-image_<version>.cmdline` - Kernel command line (includes dm-verity roothash)