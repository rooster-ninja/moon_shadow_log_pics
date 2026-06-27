#!/usr/bin/env bash
# Runner script: flash firmware, reprovision config partition, open monitor.
# Cargo passes the ELF binary path as $@ — forward it to espflash.
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")/.." && pwd)"

espflash flash --partition-table "$SCRIPT_DIR/partitions.csv" --no-reset "$@"
python3 "$SCRIPT_DIR/tools/provision.py" --flash
espflash monitor  # press Ctrl+R here to boot
