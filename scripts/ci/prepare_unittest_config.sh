#!/usr/bin/env bash
set -euo pipefail

config_path="${1:-.config}"

sed -i \
  -e 's/WARN/__TEMP__/g' \
  -e 's/ERROR/WARN/g' \
  -e 's/__TEMP__/ERROR/g' \
  "${config_path}"
