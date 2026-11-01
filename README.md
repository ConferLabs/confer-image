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

## Inference engine

The image ships vLLM, started by `confer-vllm.service` with the configuration in
`/mnt/config/secrets.env`. The launcher (`/usr/local/bin/confer-vllm`) accepts only the named
variables listed at the top of that script and runs the engine under `env -i`; there is no
argument pass-through, so an engine option that is not spelled out there cannot be enabled
from the config disk.

Speculative decoding is off unless `VLLM_MTP_TOKENS` is set.

## Image normalizer

vLLM never decodes an uploaded image. `confer-image-normalizer` (`image-normalizer/`, Rust,
JPEG/PNG/GIF/WebP decoders only) decodes them and writes width, height and 8-bit RGB. The proxy
checks those dimensions and the exact length, writes the PPM header itself and serves the
result to vLLM, so a compromised decoder can choose only pixel values. Images stream through:
the proxy sends each image in chunks as it decrypts it and copies the pixels to vLLM as they
arrive, holding neither in memory.

`confer-image-normalizer.socket` starts one process per connection, as its own dynamic user,
with no network, an empty root directory and a fixed set of system calls. The proxy keeps one
connection for each client websocket and sends only that websocket's images on it, several at a
time. Each image is decoded in a fresh child that never holds the connection and installs a
narrower filter (read, write, memory, exit) before reading it; the parent relays bytes with
splice(2) and never reads image data. So a decoder exploit reaches neither another user's images
nor the same user's other images.

The binary is built statically in `mkosi.build.chroot` with `cargo --locked`. Its tests include
Linux-only checks of the child's filter and the connection protocol (`cargo test` on Linux).
`image-normalizer/tests/compare_with_vllm.py` compares its pixels with vLLM's own image
loading.

`vllm-patches/` holds unified diffs against the pinned vLLM release, applied at build time
(`mkosi.postinst`) with no fuzz; `hashes.txt` lists every patched file with its stock and patched
sha256, so a vLLM upgrade that touches one of them fails the build rather than silently dropping
the change (`make check-vllm-patches` re-verifies the set against the wheel on PyPI). They exist
because confidential-computing guests cannot serve GPU reads of pinned host memory in place and
turn every host-device copy into a blocking call, which breaks and then serialises vLLM's V2
model runner. 
