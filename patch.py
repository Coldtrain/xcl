
#!/usr/bin/env python3
"""
Patch GTA VI intentional null writes and identified debug breakpoints.

Usage:
    python patch_null_write.py game_scarlett_master.exe

Creates:
    game_scarlett_master.exe.pre_nullwrite_patch.bak
"""

import struct
import sys
import shutil
from pathlib import Path


NOP = b"\x90"

# Only verified instruction locations and byte signatures.
PATCHES = [
    {
        "name": "Null write #1 (0x4D8BD421)",
        "rva": 0x23CF683,
        "expected": bytes.fromhex(
            "C7 04 25 00 00 00 00 21 D4 8B 4D"
        ),
    },
    {
        "name": "Null write #2",
        "rva": 0x2215C35,
        "expected": bytes.fromhex(
            "C7 04 25 00 00 00 00 00 00 00 00"
        ),
    },
    {
        "name": "Null write #3",
        "rva": 0x2215DA9,
        "expected": bytes.fromhex(
            "C7 04 25 00 00 00 00 00 00 00 00"
        ),
    },
    {
        "name": "Debug breakpoint #1",
        "rva": 0x2215C40,
        "expected": b"\xCC",
    },
    {
        "name": "Debug breakpoint #2",
        "rva": 0x2215DB6,
        "expected": b"\xCC",
    },
]


def rva_to_offset(data, rva, length):
    if len(data) < 0x40 or data[:2] != b"MZ":
        raise RuntimeError("Invalid PE file")

    pe = struct.unpack_from("<I", data, 0x3C)[0]

    if pe + 24 > len(data):
        raise RuntimeError("Invalid PE header offset")

    if data[pe:pe + 4] != b"PE\0\0":
        raise RuntimeError("Invalid PE signature")

    nsec = struct.unpack_from("<H", data, pe + 6)[0]
    optsz = struct.unpack_from("<H", data, pe + 20)[0]
    sec0 = pe + 24 + optsz

    if sec0 + nsec * 40 > len(data):
        raise RuntimeError("Section table exceeds file size")

    for i in range(nsec):
        name, vsize, va, rawsz, rawptr = struct.unpack_from(
            "<8sIIII", data, sec0 + 40 * i
        )

        if va <= rva < va + max(vsize, rawsz):
            delta = rva - va

            if delta + length > rawsz:
                raise RuntimeError(
                    f"RVA {rva:#x} is not fully file-backed"
                )

            offset = rawptr + delta

            if offset + length > len(data):
                raise RuntimeError(
                    f"RVA {rva:#x} maps outside the executable"
                )

            return offset

    raise RuntimeError(f"Could not map RVA {rva:#x}")


def main(filename):
    path = Path(filename)

    if not path.is_file():
        raise RuntimeError(f"File not found: {path}")

    data = bytearray(path.read_bytes())

    backup = path.with_suffix(
        path.suffix + ".pre_nullwrite_patch.bak"
    )

    if backup.exists():
        raise RuntimeError(f"Backup already exists: {backup}")

    resolved = []

    print(f"Executable: {path}")
    print()

    # Verify EVERY patch before changing anything.
    for patch in PATCHES:
        rva = patch["rva"]
        expected = patch["expected"]
        offset = rva_to_offset(data, rva, len(expected))
        actual = bytes(data[offset:offset + len(expected)])

        print(f"[{patch['name']}]")
        print(f"  RVA:      {rva:#x}")
        print(f"  File:     {offset:#x}")
        print(f"  Expected: {expected.hex(' ')}")
        print(f"  Actual:   {actual.hex(' ')}")

        if actual != expected:
            raise RuntimeError(
                f"Verification failed at RVA {rva:#x}. "
                "Nothing has been patched."
            )

        replacement = NOP * len(expected)

        resolved.append((offset, replacement, patch))
        print("  Verified")
        print()

    # Ensure no patches overlap.
    ranges = sorted(
        (offset, offset + len(replacement))
        for offset, replacement, _ in resolved
    )

    for previous, current in zip(ranges, ranges[1:]):
        if previous[1] > current[0]:
            raise RuntimeError("Overlapping patch locations")

    # All signatures passed. Create backup before modifying.
    shutil.copy2(path, backup)

    for offset, replacement, patch in resolved:
        data[offset:offset + len(replacement)] = replacement

    path.write_bytes(data)

    print("=" * 60)
    print("ALL PATCHES APPLIED SUCCESSFULLY")
    print("=" * 60)
    print(f"Patched locations: {len(resolved)}")
    print(f"Backup: {backup}")

    for _, replacement, patch in resolved:
        print(
            f"  {patch['name']}: "
            f"{patch['rva']:#x} -> {replacement.hex(' ')}"
        )


if __name__ == "__main__":
    if len(sys.argv) != 2:
        sys.exit(__doc__)

    try:
        main(sys.argv[1])
    except Exception as exc:
        print(f"\nERROR: {exc}", file=sys.stderr)
        sys.exit(1)
