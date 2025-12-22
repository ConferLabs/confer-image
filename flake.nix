{
  description = "Confer Confidential VM Image Builder - Reproducible builds for TDX and SEV-SNP";

  inputs = {
    nixpkgs.url = "github:nixos/nixpkgs/nixos-unstable";
    flake-utils.url = "github:numtide/flake-utils";
  };

  outputs = { self, nixpkgs, flake-utils }:
    flake-utils.lib.eachDefaultSystem (system:
      let
        pkgs = import nixpkgs {
          inherit system;
          config.allowUnfree = true; # For CUDA if needed
        };

        # Use mkosi with QEMU support (includes systemdForMkosi with repart, ukify, etc.)
        mkosi-with-qemu = pkgs.mkosi-full;

      in {
        devShells.default = pkgs.mkShell {
          name = "confer-cvm-builder";

          buildInputs = [
            # Core build tools - mkosi with QEMU support
            mkosi-with-qemu
            pkgs.qemu
            pkgs.qemu-utils

            # Filesystem tools required by mkosi (not included in mkosi package)
            pkgs.dosfstools    # mkfs.vfat for ESP
            pkgs.e2fsprogs     # mkfs.ext4 with SOURCE_DATE_EPOCH support (>= 1.47.1)
            pkgs.cryptsetup    # veritysetup for dm-verity
            pkgs.squashfsTools # mksquashfs
            pkgs.mtools        # mcopy for FAT filesystem operations

            # Ubuntu/Debian package management (for mkosi to install packages)
            pkgs.apt
            pkgs.dpkg
            pkgs.debootstrap
            pkgs.gnupg

            # Python tooling (for build scripts)
            pkgs.python312
            pkgs.python312Packages.pip
            pkgs.python312Packages.virtualenv

            # XML libraries for lxml compilation (needed by nv-attestation-sdk)
            pkgs.libxml2
            pkgs.libxslt

            # Utilities
            pkgs.git
            pkgs.gnumake
            pkgs.coreutils
            pkgs.util-linux
            pkgs.binutils  # Provides objcopy for UKI extraction
            pkgs.gzip
            pkgs.xz
            pkgs.zstd
          ];

          shellHook = ''
            # Ensure python3 points to Python 3.12 (required for lxml compatibility)
            export PATH="${pkgs.python312}/bin:$PATH"

            echo "Confer Confidential VM Image Builder"
            echo "====================================="
            echo ""
            echo "Available commands:"
            echo "  make build           - Build confidential VM image (TDX/SEV-SNP)"
            echo "  make clean           - Clean build artifacts"
            echo ""
            echo "mkosi version: $(mkosi --version)"
            echo "python3 version: $(python3 --version)"
          '';
        };

        # For CI/CD
        packages.default = mkosi-with-qemu;
      }
    );
}
