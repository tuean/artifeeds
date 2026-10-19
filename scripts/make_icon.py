#!/usr/bin/env python3
"""Generate the Artifeeds app icon — no image libraries required.

Draws the icon analytically (signed distance fields, so edges are properly
antialiased at every size) and writes PNGs directly, then, on macOS, packs them
into an `.icns` with `iconutil`.

    python3 scripts/make_icon.py            # assets/AppIcon.iconset + .icns
    python3 scripts/make_icon.py --png-only # skip iconutil

The mark is the feed glyph — a dot and two broadcast arcs — in the app's accent
colour on the app's dark panel, with a soft gradient and an inner highlight so it
does not look flat next to native icons.
"""

from __future__ import annotations

import argparse
import math
import os
import struct
import subprocess
import sys
import zlib

# Palette, matching crates/artifeeds-app/src/theme.rs.
BG_TOP = (0x16, 0x1D, 0x29)
BG_BOTTOM = (0x0B, 0x0F, 0x16)
ACCENT = (0x7A, 0xA2, 0xF7)
ACCENT_DEEP = (0x4A, 0x74, 0xD0)
EDGE = (0x2A, 0x36, 0x4A)

# Every size macOS wants in an iconset.
ICONSET = [
    (16, "icon_16x16.png"),
    (32, "icon_16x16@2x.png"),
    (32, "icon_32x32.png"),
    (64, "icon_32x32@2x.png"),
    (128, "icon_128x128.png"),
    (256, "icon_128x128@2x.png"),
    (256, "icon_256x256.png"),
    (512, "icon_256x256@2x.png"),
    (512, "icon_512x512.png"),
    (1024, "icon_512x512@2x.png"),
]


# ---------------------------------------------------------------------------
# PNG output
# ---------------------------------------------------------------------------

def write_png(path: str, width: int, height: int, rgba: bytearray) -> None:
    stride = width * 4
    raw = bytearray()
    for y in range(height):
        raw.append(0)  # filter: none
        raw += rgba[y * stride : (y + 1) * stride]

    def chunk(tag: bytes, data: bytes) -> bytes:
        return (
            struct.pack(">I", len(data))
            + tag
            + data
            + struct.pack(">I", zlib.crc32(tag + data) & 0xFFFFFFFF)
        )

    png = b"\x89PNG\r\n\x1a\n"
    png += chunk(b"IHDR", struct.pack(">IIBBBBB", width, height, 8, 6, 0, 0, 0))
    png += chunk(b"IDAT", zlib.compress(bytes(raw), 9))
    png += chunk(b"IEND", b"")
    with open(path, "wb") as f:
        f.write(png)


# ---------------------------------------------------------------------------
# shapes (all in a 0..1 unit square; distances are scaled to pixels)
# ---------------------------------------------------------------------------

def sd_round_rect(px: float, py: float, half: float, radius: float) -> float:
    dx = abs(px) - (half - radius)
    dy = abs(py) - (half - radius)
    ax, ay = max(dx, 0.0), max(dy, 0.0)
    return math.hypot(ax, ay) + min(max(dx, dy), 0.0) - radius


def sd_circle(px: float, py: float, cx: float, cy: float, radius: float) -> float:
    return math.hypot(px - cx, py - cy) - radius


def sd_arc(px: float, py: float, cx: float, cy: float, radius: float, width: float) -> float:
    """Distance to the quarter arc that sweeps from "right" to "up".

    Image coordinates have y pointing down, so "up" is -y and the sweep is
    -90°..0°. Clamping the angle to that range is what gives the arc its round
    caps: outside the sweep the nearest point on the arc is an endpoint.
    """
    angle = math.atan2(py - cy, px - cx)
    clamped = min(max(angle, -math.pi / 2), 0.0)
    qx = cx + radius * math.cos(clamped)
    qy = cy + radius * math.sin(clamped)
    return math.hypot(px - qx, py - qy) - width / 2.0


def coverage(distance: float, softness: float) -> float:
    """1 inside, 0 outside, smooth across ~1.5px."""
    return min(max(0.5 - distance / softness, 0.0), 1.0)


def soft_falloff(distance: float, reach: float) -> float:
    """A wide, smooth ramp from 1 inside to 0 outside — a shadow, not an edge."""
    t = min(max(0.5 - distance / reach, 0.0), 1.0)
    return t * t * (3.0 - 2.0 * t)


def mix(a, b, t: float):
    return tuple(a[i] + (b[i] - a[i]) * t for i in range(3))


def over(dst: list, src_rgb, alpha: float) -> None:
    """Composite `src_rgb` at `alpha` over the pixel in `dst` (premultiplied-free)."""
    if alpha <= 0.0:
        return
    for i in range(3):
        dst[i] = dst[i] * (1.0 - alpha) + src_rgb[i] * alpha
    dst[3] = dst[3] * (1.0 - alpha) + alpha


# macOS lays app icons out on a fixed grid: the artwork ("squircle") is
# 824x1024 of the canvas, centred, so every icon in the Dock occupies the same
# optical area. Filling the canvas instead — which is what this file used to do —
# makes the icon look a size larger than everything beside it.
BODY = 824.0 / 1024.0
BODY_RADIUS = 0.2237  # corner radius as a fraction of the body, not the canvas

# Apple's icons carry a baked drop shadow: the plate is 824 wide but the *visible*
# extent is ~864, and it is not symmetric — the shadow falls downwards. Without
# it an icon measures correctly and still reads small next to its neighbours,
# because every other icon in the Dock has this halo. Measured against Mail,
# Terminal and Finder: solid plate 80.5 % of the canvas, visible 84.4 %, with the
# top margin 8.6 % and the bottom 7.0 %.
SHADOW_OFFSET_Y = 9.0 / 1024.0
SHADOW_REACH = 62.0 / 1024.0
SHADOW_ALPHA = 0.36
SHADOW_RGB = (0.0, 0.0, 0.0)


def render(size: int) -> bytearray:
    """Render one icon at `size`x`size` pixels."""
    out = bytearray(size * size * 4)
    # Geometry in unit space (canvas = 1.0), then multiplied by `size`.
    half = BODY / 2.0
    radius = BODY_RADIUS * BODY
    # The glyph keeps its position *on the plate*: the same fractions of the
    # body as before, so the artwork is unchanged and only the plate moved in.
    body_x = 0.5 - half
    glyph_cx = body_x + BODY * 0.31
    glyph_cy = body_x + BODY * 0.705
    stroke = BODY * 0.085
    softness = 1.6

    for y in range(size):
        # Pixel centre in unit coordinates.
        py = (y + 0.5) / size
        for x in range(size):
            px = (x + 0.5) / size
            # Background plate, preceded by its shadow: the same rounded rect,
            # nudged down and blurred wide, which is what makes the icon read at
            # the same size as the ones beside it.
            d = sd_round_rect(px - 0.5, py - 0.5, half, radius)
            pixel = [0.0, 0.0, 0.0, 0.0]
            shadow_d = sd_round_rect(px - 0.5, py - 0.5 - SHADOW_OFFSET_Y, half, radius)
            shadow = soft_falloff(shadow_d * size, SHADOW_REACH * size) * SHADOW_ALPHA
            if shadow > 0.0:
                over(pixel, SHADOW_RGB, shadow)

            alpha = coverage(d * size, softness)
            if alpha > 0.0:
                plate = mix(BG_TOP, BG_BOTTOM, py)
                # A hairline inner edge reads as depth at large sizes.
                edge = coverage((abs(d) - 0.004 * BODY) * size, softness) * 0.55
                plate = mix(plate, EDGE, edge)
                over(pixel, plate, alpha)

            # The glyph is clipped to the plate: with the artwork inset to the
            # macOS grid the outer arc reaches past the rounded corner, and a
            # stroke floating on the transparent margin would also stretch the
            # image's bounds past the 824 the grid asks for.
            if alpha > 0.0:
                # Broadcast arcs, drawn outward from the corner dot.
                for arc_radius in (0.255, 0.425):
                    da = sd_arc(px, py, glyph_cx, glyph_cy, arc_radius * BODY, stroke)
                    ca = coverage(da * size, softness) * alpha
                    if ca > 0.0:
                        over(pixel, mix(ACCENT, ACCENT_DEEP, arc_radius), ca)

                # The dot.
                dd = sd_circle(px, py, glyph_cx, glyph_cy, stroke * 0.62)
                cd = coverage(dd * size, softness) * alpha
                if cd > 0.0:
                    over(pixel, ACCENT_DEEP, cd)

            i = (y * size + x) * 4
            out[i] = int(round(min(max(pixel[0], 0.0), 255.0)))
            out[i + 1] = int(round(min(max(pixel[1], 0.0), 255.0)))
            out[i + 2] = int(round(min(max(pixel[2], 0.0), 255.0)))
            out[i + 3] = int(round(min(max(pixel[3], 0.0), 1.0) * 255))
    return out


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("--out", default="assets", help="output directory")
    ap.add_argument("--png-only", action="store_true", help="do not run iconutil")
    args = ap.parse_args()

    iconset = os.path.join(args.out, "AppIcon.iconset")
    os.makedirs(iconset, exist_ok=True)

    cache: dict[int, bytearray] = {}
    for size, name in ICONSET:
        if size not in cache:
            cache[size] = render(size)
        write_png(os.path.join(iconset, name), size, size, cache[size])
        print(f"  {name:24} {size}x{size}")

    icns = os.path.join(args.out, "AppIcon.icns")
    if args.png_only or sys.platform != "darwin":
        print(f"skipped icns ({sys.platform}); iconset at {iconset}")
        return 0

    subprocess.run(["iconutil", "-c", "icns", iconset, "-o", icns], check=True)
    print(f"wrote {icns} ({os.path.getsize(icns)} bytes)")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
