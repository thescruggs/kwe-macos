#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0
# Bounded shared-frame-file reader used by the smoke suites (smoke-video.sh
# keeps its own inline copy; smoke-web.sh uses this one).
#
# Protocol (crates/kwe-frame-protocol/src/lib.rs): 64-byte little-endian
# header, two BGRA8888 slots, generation-toggle publishing. A "stable
# snapshot" is read with the writer's own bounded discipline: read the
# generation, sample a slot, re-read the generation, retry up to 64 times.
#
# Usage:
#   frame-read.py <frame-file> [x y]         sample pixel (x, y) as R,G,B
#   frame-read.py <frame-file> probe <x y w h> [r g b] [tolerance]
#       assert a pixel in the w x h box starting at (x, y) matches
#       (r, g, b) within tolerance (any pixel in the box may match)
#   frame-read.py <frame-file> baseline <x y w h>
#       print all 3-byte pixels of the box, one per line (for diffing)
#
# The probe/baseline coordinates are raw pixel coordinates of the frame
# buffer (the smoke computes them from the same viewport math as the
# renderer). Every bound mirrors the Rust constants: header bytes, slot
# count, stride, and dimension caps are validated before any pixel work.
import struct
import sys

MAGIC = b"KWEFRM1\0"
HEADER_BYTES = 64
SLOT_COUNT = 2
PIXEL_FORMAT_BGRA = 1
MAX_DIMENSION = 8192
MAX_SNAPSHOT_ATTEMPTS = 64

# (offset, fmt) pairs for the little-endian header.
FIELDS = [
    ("version", 8, "<I"),
    ("header_bytes", 12, "<I"),
    ("file_bytes", 16, "<Q"),
    ("width", 24, "<I"),
    ("height", 28, "<I"),
    ("stride", 32, "<I"),
    ("pixel_format", 36, "<I"),
    ("slot_count", 40, "<I"),
    ("generation", 48, "<Q"),
    ("active_slot", 56, "<I"),
    ("producer_state", 60, "<I"),
]


class FrameError(Exception):
    pass


def read_header(data):
    if len(data) < HEADER_BYTES:
        raise FrameError("file smaller than the 64-byte header")
    if data[0:8] != MAGIC:
        raise FrameError("bad magic (not a shared frame file)")
    header = {}
    for name, offset, fmt in FIELDS:
        (value,) = struct.unpack_from(fmt, data, offset)
        header[name] = value
    if header["version"] != 1:
        raise FrameError("unsupported frame protocol version %d" % header["version"])
    if header["header_bytes"] != HEADER_BYTES:
        raise FrameError("unexpected header size %d" % header["header_bytes"])
    if header["pixel_format"] != PIXEL_FORMAT_BGRA:
        raise FrameError("unexpected pixel format %d" % header["pixel_format"])
    if header["slot_count"] != SLOT_COUNT:
        raise FrameError("unexpected slot count %d" % header["slot_count"])
    if header["width"] == 0 or header["height"] == 0:
        raise FrameError("zero dimension")
    if header["width"] > MAX_DIMENSION or header["height"] > MAX_DIMENSION:
        raise FrameError("dimensions exceed the protocol cap")
    if header["stride"] != header["width"] * 4:
        raise FrameError("stride does not match width * 4")
    expected = HEADER_BYTES + SLOT_COUNT * header["stride"] * header["height"]
    if header["file_bytes"] != expected or len(data) < expected:
        raise FrameError("file size does not match the header")
    return header


def snapshot(path):
    """Read one stable (generation-even, re-verified) snapshot. Mirrors the
    writer's own publication discipline: retries are bounded."""
    for _ in range(MAX_SNAPSHOT_ATTEMPTS):
        with open(path, "rb") as f:
            data = f.read()
        header = read_header(data)
        if header["generation"] % 2 != 0:
            continue  # publish in progress; retry
        slot = header["active_slot"]
        if slot >= SLOT_COUNT:
            raise FrameError("active slot out of range")
        offset = HEADER_BYTES + slot * header["stride"] * header["height"]
        pixels = data[offset : offset + header["stride"] * header["height"]]
        with open(path, "rb") as f:
            data2 = f.read()
        header2 = read_header(data2)
        if (
            header2["generation"] != header["generation"]
            or header2["active_slot"] != slot
        ):
            continue  # writer advanced mid-read; retry
        return header2, pixels
    raise FrameError("no stable snapshot within the retry bound")


def pixel_bgra(pixels, header, x, y):
    if x >= header["width"] or y >= header["height"]:
        raise FrameError("pixel out of bounds")
    offset = y * header["stride"] + x * 4
    return (pixels[offset + 2], pixels[offset + 1], pixels[offset])  # R, G, B


def main():
    if len(sys.argv) < 3:
        sys.stderr.write(__doc__)
        return 1
    path = sys.argv[1]
    mode = sys.argv[2]
    header, pixels = snapshot(path)
    if mode == "probe":
        if len(sys.argv) < 6:
            sys.stderr.write("probe needs <x y w h> [r g b] [tolerance]\n")
            return 2
        x, y, w, h = (int(v) for v in sys.argv[3:7])
        target = tuple(int(v) for v in sys.argv[7:10]) if len(sys.argv) >= 10 else None
        tolerance = int(sys.argv[10]) if len(sys.argv) >= 11 else 0
        for py in range(y, y + h):
            for px in range(x, x + w):
                rgb = pixel_bgra(pixels, header, px, py)
                if target is None or all(
                    abs(rgb[i] - target[i]) <= tolerance for i in range(3)
                ):
                    print("PROBE-OK %dx%d generation=%d state=%d pixel=%d,%d=%s" % (
                        header["width"], header["height"], header["generation"],
                        header["producer_state"], px, py, rgb))
                    return 0
        print("PROBE-FAIL generation=%d state=%d" % (
            header["generation"], header["producer_state"]))
        return 1
    if mode == "baseline":
        if len(sys.argv) < 6:
            sys.stderr.write("baseline needs <x y w h>\n")
            return 2
        x, y, w, h = (int(v) for v in sys.argv[3:7])
        for py in range(y, y + h):
            for px in range(x, x + w):
                rgb = pixel_bgra(pixels, header, px, py)
                print("%d,%d,%d" % rgb)
        return 0
    if mode == "pixel":
        if len(sys.argv) < 5:
            sys.stderr.write("pixel needs <x y>\n")
            return 2
        x, y = int(sys.argv[3]), int(sys.argv[4])
        rgb = pixel_bgra(pixels, header, x, y)
        print("PIXEL %d,%d,%d generation=%d state=%d" % (
            rgb[0], rgb[1], rgb[2], header["generation"], header["producer_state"]))
        return 0
    sys.stderr.write("unknown mode %r\n" % mode)
    return 2


if __name__ == "__main__":
    sys.exit(main())
