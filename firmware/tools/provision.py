#!/usr/bin/env python3
"""
Generate config.bin and flash it to the moon_shadow_photo config partition.

Usage:
    python3 tools/provision.py            # generate config.bin only
    python3 tools/provision.py --flash    # generate AND flash (device must be connected)
    python3 tools/provision.py --port /dev/ttyUSB0 --flash

Binary format written to 0x3F0000:
    [4 bytes magic][4 bytes JSON length, LE][N bytes JSON]
"""

import argparse
import json
import struct
import subprocess
import sys

# ── Edit these before running ─────────────────────────────────────────────────
CONFIG = {
    "wifi_ssid":    "Shop",
    "wifi_pass":    "Oligarchy",
    "mqtt_host":    "10.0.0.50",
    "mqtt_port":    1883,
    "mqtt_user":    "",
    "mqtt_pass":    "",
    "device_id":    "moon-shadow-001",
    "framesize":    9,         # 9=SVGA(800x600), 10=XGA(1024x768), 12=SXGA, 13=UXGA(1600x1200)
    "jpeg_quality": 10,        # 0-63; lower = higher quality
    "gain_ceiling": 0,         # 0=2x, 1=4x, 2=8x, 3=16x, 4=32x, 5=64x, 6=128x max auto gain
    "upload_host":  "10.0.0.50",
    "upload_port":  8765,
}
# ─────────────────────────────────────────────────────────────────────────────

MAGIC       = bytes([0xFA, 0x12, 0xC3, 0x7A])
FLASH_ADDR  = 0x3F0000
SECTOR_SIZE = 0x1000
OUTPUT_FILE = "config.bin"


def build_bin(cfg: dict) -> bytes:
    payload = json.dumps(cfg, separators=(",", ":")).encode("utf-8")
    if len(payload) > 512:
        raise ValueError(f"Config JSON too large ({len(payload)} bytes, max 512)")
    header = MAGIC + struct.pack("<I", len(payload))
    return header + payload


def main():
    parser = argparse.ArgumentParser(description=__doc__,
                                     formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--flash", action="store_true",
                        help="Erase the config partition and flash config.bin after building it")
    parser.add_argument("--port", default=None,
                        help="Serial port (e.g. /dev/ttyUSB0). Auto-detected if omitted.")
    args = parser.parse_args()

    data = build_bin(CONFIG)
    with open(OUTPUT_FILE, "wb") as f:
        f.write(data)
    print(f"config.bin written ({len(data)} bytes)")
    print(f"  JSON payload: {data[8:].decode()}")

    if not args.flash:
        print(f"\nTo flash manually:")
        print(f"  espflash erase-region {hex(FLASH_ADDR)} {hex(SECTOR_SIZE)}")
        print(f"  espflash write-bin {hex(FLASH_ADDR)} {OUTPUT_FILE}")
        return

    port_args = ["--port", args.port] if args.port else []

    print(f"\nErasing config partition at {hex(FLASH_ADDR)}…")
    subprocess.run(
        ["espflash", "erase-region"] + port_args +
        [hex(FLASH_ADDR), hex(SECTOR_SIZE)],
        check=True,
    )

    print(f"Writing {OUTPUT_FILE} to {hex(FLASH_ADDR)}…")
    subprocess.run(
        ["espflash", "write-bin"] + port_args +
        [hex(FLASH_ADDR), OUTPUT_FILE],
        check=True,
    )

    print("Done. Reset the device to apply the new config.")


if __name__ == "__main__":
    main()
