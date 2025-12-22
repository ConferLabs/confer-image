.PHONY: help build clean freeze-requirements extract-uki extract-boot-artifacts

# Image version - can be overridden via IMAGE_VERSION=x.y.z, defaults to mkosi.conf
IMAGE_VERSION ?= $(shell grep '^ImageVersion=' mkosi.conf | cut -d= -f2)

# Output files (mkosi adds version suffix from ImageVersion in mkosi.conf)
DISK_IMAGE_RAW=confer-image_$(IMAGE_VERSION).raw
DISK_IMAGE=confer-image_$(IMAGE_VERSION).qcow2
UKI_IMAGE=confer-image_$(IMAGE_VERSION).efi
KERNEL_IMAGE=confer-image_$(IMAGE_VERSION).vmlinuz
INITRD_IMAGE=confer-image_$(IMAGE_VERSION).initrd
CMDLINE_FILE=confer-image_$(IMAGE_VERSION).cmdline

# Default target
help:
	@echo "Confer Confidential VM Image Builder"
	@echo "====================================="
	@echo ""
	@echo "Targets:"
	@echo "  make build                  - Build confidential VM disk image (TDX/SEV-SNP)"
	@echo "  make clean                  - Remove build artifacts"
	@echo "  make freeze-requirements    - Generate locked Python requirements"
	@echo "  make extract-uki            - Extract UKI from disk image ESP"
	@echo "  make extract-boot-artifacts - Extract kernel, initrd, cmdline for direct boot"
	@echo ""
	@echo "Output (for direct kernel boot):"
	@echo "  $(DISK_IMAGE)    - Rootfs disk image with dm-verity"
	@echo "  $(KERNEL_IMAGE)  - Linux kernel for -kernel flag"
	@echo "  $(INITRD_IMAGE)  - Initrd for -initrd flag"
	@echo "  $(CMDLINE_FILE)  - Base kernel cmdline (add proxy-hash at runtime)"
	@echo ""
	@echo "Prerequisites:"
	@echo "  1. Install Nix: https://nixos.org/download.html"
	@echo "  2. Enable flakes: https://nixos.wiki/wiki/Flakes"
	@echo "  3. Run: nix develop"
	@echo ""

# Build the VM disk image and generate measurements
build: mkosi.extra/requirements-vllm.lock mkosi.extra/requirements-attestation.lock
	@echo "Building confidential VM disk image with dm-verity..."
	@echo "Image version: $(IMAGE_VERSION)"
	@# Detect skeleton/config changes to decide if incremental cache should be cleared
	@SKELETON_HASH=$$(find mkosi.skeleton mkosi.conf -type f -exec sha256sum {} \; 2>/dev/null | sort | sha256sum | cut -d' ' -f1); \
	if [ "$$SKELETON_HASH" != "$$(cat .skeleton-hash 2>/dev/null)" ]; then \
		echo "Skeleton/config changed, clearing incremental cache..."; \
		sudo $$(which mkosi) --force --force --image-version=$(IMAGE_VERSION); \
		echo "$$SKELETON_HASH" > .skeleton-hash; \
	else \
		echo "Skeleton unchanged, using incremental cache..."; \
		sudo $$(which mkosi) --force --image-version=$(IMAGE_VERSION); \
	fi
	@echo ""
	@echo "Extracting boot artifacts for direct kernel boot..."
	@$(MAKE) extract-boot-artifacts
	@echo ""
	@echo "Converting to compressed qcow2..."
	@qemu-img convert -f raw -O qcow2 -c -o compression_type=zstd -W -m 16 \
		$(DISK_IMAGE_RAW) $(DISK_IMAGE)
	@echo "✓ Converted to $(DISK_IMAGE) ($$(du -h $(DISK_IMAGE) | cut -f1))"
	@echo ""
	@echo "=== Build Complete ==="
	@echo "Output files for direct kernel boot:"
	@echo "  $(KERNEL_IMAGE)  - Use with QEMU -kernel"
	@echo "  $(INITRD_IMAGE)  - Use with QEMU -initrd"
	@echo "  $(DISK_IMAGE)    - Use with QEMU -drive"
	@echo "  $(CMDLINE_FILE)  - Base cmdline (append proxy-hash=<hash> at runtime)"
	@echo ""

# Extract UKI from disk image ESP partition
extract-uki:
	@echo "Extracting UKI from ESP partition..."
	@if [ ! -f $(DISK_IMAGE_RAW) ]; then \
		echo "Error: Disk image not found. Run 'make build' first."; \
		exit 1; \
	fi
	@# Find ESP partition offset (partition type C12A7328-F81F-11D2-BA4B-00A0C93EC93B)
	@ESP_START=$$(sfdisk -J $(DISK_IMAGE_RAW) | python3 -c "import json,sys; parts=json.load(sys.stdin)['partitiontable']['partitions']; print(next(p['start'] for p in parts if p.get('type','').upper() == 'C12A7328-F81F-11D2-BA4B-00A0C93EC93B'))") && \
	ESP_SIZE=$$(sfdisk -J $(DISK_IMAGE_RAW) | python3 -c "import json,sys; parts=json.load(sys.stdin)['partitiontable']['partitions']; print(next(p['size'] for p in parts if p.get('type','').upper() == 'C12A7328-F81F-11D2-BA4B-00A0C93EC93B'))") && \
	echo "  ESP at sector $$ESP_START, size $$ESP_SIZE sectors" && \
	rm -rf .esp-mount && mkdir -p .esp-mount && \
	sudo mount -o loop,offset=$$((ESP_START * 512)),sizelimit=$$((ESP_SIZE * 512)),ro $(DISK_IMAGE_RAW) .esp-mount && \
	echo "  ESP contents:" && \
	find .esp-mount -type f | head -20 && \
	UKI_PATH=$$(find .esp-mount -name '*.efi' -path '*/EFI/Linux/*' | head -1) && \
	if [ -z "$$UKI_PATH" ]; then \
		UKI_PATH=$$(find .esp-mount -name '*.efi' -path '*/EFI/*' | head -1); \
	fi && \
	if [ -z "$$UKI_PATH" ]; then \
		echo "Error: Could not find UKI in ESP"; \
		sudo umount .esp-mount; \
		exit 1; \
	fi && \
	echo "  Found UKI at: $$UKI_PATH" && \
	sudo cp "$$UKI_PATH" $(UKI_IMAGE) && \
	sudo chown $$(id -u):$$(id -g) $(UKI_IMAGE) && \
	sudo umount .esp-mount && \
	rm -rf .esp-mount && \
	echo "✓ UKI extracted to $(UKI_IMAGE)"

# Extract kernel, initrd, cmdline from UKI for direct kernel boot
extract-boot-artifacts: extract-uki
	@echo "Extracting boot artifacts from UKI..."
	@if [ ! -f $(UKI_IMAGE) ]; then \
		echo "Error: UKI $(UKI_IMAGE) not found."; \
		exit 1; \
	fi
	@# Extract kernel, initrd, and cmdline sections from UKI PE binary
	objcopy --dump-section .linux=$(KERNEL_IMAGE) \
	        --dump-section .initrd=$(INITRD_IMAGE) \
	        --dump-section .cmdline=.cmdline.raw \
	        $(UKI_IMAGE)
	@# Clean up cmdline (remove null bytes) and save
	@cat .cmdline.raw | tr -d '\0' > $(CMDLINE_FILE)
	@rm -f .cmdline.raw
	@echo "✓ Kernel extracted to $(KERNEL_IMAGE) ($$(du -h $(KERNEL_IMAGE) | cut -f1))"
	@echo "✓ Initrd extracted to $(INITRD_IMAGE) ($$(du -h $(INITRD_IMAGE) | cut -f1))"
	@echo "✓ Cmdline saved to $(CMDLINE_FILE)"
	@echo "  $$(cat $(CMDLINE_FILE))"

# Copy requirements lock files
mkosi.extra/requirements-vllm.lock: requirements-vllm.lock
	@mkdir -p mkosi.extra
	@cp requirements-vllm.lock mkosi.extra/

mkosi.extra/requirements-attestation.lock: requirements-attestation.lock
	@mkdir -p mkosi.extra
	@cp requirements-attestation.lock mkosi.extra/

# Generate frozen Python requirements (run this once to create lock file)
freeze-requirements:
	@echo "Generating frozen Python requirements..."
	@echo "This will create a temporary venv and freeze all dependencies"
	@echo ""
	@echo "Installing: vllm + nv-attestation-sdk (for GPU attestation)"
	@echo ""
	@rm -rf /tmp/vllm-freeze
	@python3.12 -m venv /tmp/vllm-freeze
	@/tmp/vllm-freeze/bin/pip install --upgrade pip
	@/tmp/vllm-freeze/bin/pip install virtualenv vllm nv-attestation-sdk
	@/tmp/vllm-freeze/bin/pip freeze > requirements.lock
	@rm -rf /tmp/vllm-freeze
	@echo "Created requirements.lock with $(shell wc -l < requirements.lock) pinned packages"

# Clean build artifacts
clean:
	@echo "Cleaning build artifacts..."
	@rm -rf mkosi.output/
	@rm -rf mkosi.cache/
	@rm -rf mkosi.extra/
	@rm -rf .esp-mount/
	@rm -f $(DISK_IMAGE)
	@rm -f $(UKI_IMAGE)
	@rm -f $(KERNEL_IMAGE) $(INITRD_IMAGE) $(CMDLINE_FILE)
	@echo "Clean complete"
