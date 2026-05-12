#!/bin/sh

set -eu

config_path=${1:-.config}

if [ ! -f "$config_path" ]; then
    exit 0
fi

awk -F= '
        /^KFEAT_[A-Z][A-Z0-9_]*=y$/ {
            feature = $1
            sub(/^KFEAT_/, "", feature)
            print tolower(feature)
        }
        /^PLATFORM_[A-Z][A-Z0-9_]*=y$/ {
            platform = $1
            sub(/^PLATFORM_/, "", platform)
            print "platform_" tolower(platform)
        }
    ' "$config_path" | sort -u | paste -sd' ' -
