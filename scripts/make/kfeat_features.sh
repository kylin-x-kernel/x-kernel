#!/bin/sh

set -eu

config_path=${1:-.config}

if [ ! -f "$config_path" ]; then
    exit 0
fi

# Map every `KFEAT_<NAME>=y` in .config to a cargo feature `<name>` enabled on
# the `kfeat` crate.
#
# Some `KFEAT_*` keys are pure build-time / Make-side knobs (consumed only by
# Makefile glue or by `kbuild_config` constants), and intentionally do NOT have
# a matching cargo feature on `kfeat`. Skip those so cargo does not fail with
# "kfeat does not have that feature".
awk -F= '
        function is_kconfig_only_feature(key) {
            return key == "KFEAT_FS" ||
                   key == "KFEAT_VIRTIO_BUS_PCI" ||
                   key == "KFEAT_VIRTIO_BUS_MMIO"
        }

        /^KFEAT_[A-Z][A-Z0-9_]*=y$/ {
            key = $1
            # Build-time-only Kconfig keys: not exposed as kfeat features.
            if (is_kconfig_only_feature(key)) {
                next
            }
            feature = key
            sub(/^KFEAT_/, "", feature)
            print tolower(feature)
        }
        /^ARCH_[A-Z][A-Z0-9_]*=y$/ {
            arch = $1
            sub(/^ARCH_/, "", arch)
            # Arch HAL crate feature. The old PLATFORM_KPLAT_ARCH symbol was a
            # redundant 1:1 echo of ARCH and has been removed; the crate is now
            # selected by ARCH directly. The feature name stays platform_kplat_arch
            # so api/kfeat (Cargo.toml plus extern crate) needs no change. This
            # must mirror xtask/xconfig gen_cargo extract_kfeat_features.
            if (arch == "AARCH64") print "platform_kplat_aarch64"
            else if (arch == "X86_64") print "platform_kplat_x86_64"
            else if (arch == "RISCV64") print "platform_kplat_riscv64"
            else if (arch == "LOONGARCH64") print "platform_kplat_loongarch64"
        }
    ' "$config_path" | sort -u | paste -sd' ' -
