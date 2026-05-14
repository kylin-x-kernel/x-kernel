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
        /^KFEAT_[A-Z][A-Z0-9_]*=y$/ {
            key = $1
            # Build-time-only Kconfig keys: not exposed as kfeat features.
            if (key == "KFEAT_VIRTIO_BUS_PCI" || key == "KFEAT_VIRTIO_BUS_MMIO") {
                next
            }
            feature = key
            sub(/^KFEAT_/, "", feature)
            print tolower(feature)
        }
        /^PLATFORM_[A-Z][A-Z0-9_]*=y$/ {
            platform = $1
            sub(/^PLATFORM_/, "", platform)
            print "platform_" tolower(platform)
        }
    ' "$config_path" | sort -u | paste -sd' ' -
