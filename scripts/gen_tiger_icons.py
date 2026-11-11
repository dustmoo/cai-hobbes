#!/usr/bin/env python3
"""Draw the pixel tiger into the app icons.

Reads TIGER_GRID and its palette from src/components/pixel_icons.rs (so the
icon stays in sync with the in-app mascot), then, for each PNG in
assets/icons/hobbes.iconset and assets/icons/hobbes-pro.iconset:

  1. finds the circle (scanning in from the plate's edges, so a re-run finds
     the same circle whether the old sparkle or a tiger sits inside it),
  2. repaints the circle's fill (erasing the sparkle or a previous tiger),
  3. draws the tiger with crisp, whole-pixel cells, centred, with a one-cell
     keyline (dark on the pale standard circle, cream on the dark Pro one).

Below 32 px a cell would be under a pixel, so the grid is reduced 2x2 to an
8x8 tiger (still derived from TIGER_GRID). Then it writes assets/icon.icns
and assets/icon-pro.icns (iconutil), assets/icon.ico and assets/icon-pro.ico
(16-256, BMP entries plus a PNG 256), and assets/favicon.png.

Pure standard library (zlib PNG codec), so no Pillow needed:

    python3 -I scripts/gen_tiger_icons.py [--preview DIR]

--preview DIR writes 512 px previews of both editions plus 32 and 16 px
blow-ups there and touches nothing else.
"""

import os
import re
import shutil
import struct
import subprocess
import sys
import tempfile
import zlib
from collections import Counter

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
PIXEL_ICONS = os.path.join(ROOT, "src", "components", "pixel_icons.rs")
EDITIONS = {
    # iconset dir, .icns, .ico, keyline palette cell
    "standard": ("assets/icons/hobbes.iconset", "assets/icon.icns", "assets/icon.ico", "s"),
    "pro": ("assets/icons/hobbes-pro.iconset", "assets/icon-pro.icns", "assets/icon-pro.ico", "c"),
}
OUTLINE = "k"  # the keyline's grid character (not in the Rust grid)

# ── Source: the grid and palette ────────────────────────────────────────────


def load_tiger():
    src = open(PIXEL_ICONS, encoding="utf-8").read()
    m = re.search(r"pub const TIGER_GRID: \[&str; 16\] = \[(.*?)\];", src, re.S)
    if not m:
        sys.exit("TIGER_GRID not found in pixel_icons.rs")
    grid = re.findall(r'"([^"]*)"', m.group(1))
    assert len(grid) == 16 and all(len(r) == 16 for r in grid), "TIGER_GRID must be 16x16"
    # 'o' => Self::Body  …  and  Self::Body => Some("#E8913A")
    chars = dict(re.findall(r"'(\w)' => Self::(\w+)", src))
    hexes = dict(re.findall(r'Self::(\w+) => Some\("#([0-9A-Fa-f]{6})"\)', src))
    palette = {}
    for ch, name in chars.items():
        h = hexes[name]
        palette[ch] = tuple(int(h[i : i + 2], 16) for i in (0, 2, 4)) + (255,)
    return grid, palette


def reduce_grid(grid):
    """16x16 -> 8x8 by 2x2 blocks: an eye anywhere wins, a block needs two
    filled cells, otherwise the commonest cell, lighter on a tie (so the
    reduction stays symmetric)."""
    order = "ocbse"
    out = []
    for by in range(8):
        row = ""
        for bx in range(8):
            cells = [grid[y][x] for y in (2 * by, 2 * by + 1) for x in (2 * bx, 2 * bx + 1)]
            filled = [c for c in cells if c != "."]
            if "e" in cells:
                row += "e"
            elif len(filled) < 2:
                row += "."
            else:
                counts = Counter(filled)
                row += max(counts, key=lambda c: (counts[c], -order.index(c)))
        out.append(row)
    return out


def with_outline(grid):
    """Pad by one cell and ring the silhouette (4-neighbour) with OUTLINE."""
    n = len(grid) + 2
    g = [["."] * n for _ in range(n)]
    for y, row in enumerate(grid):
        for x, c in enumerate(row):
            g[y + 1][x + 1] = c
    filled = {(x, y) for y in range(n) for x in range(n) if g[y][x] != "."}
    for x, y in list(filled):
        for dx, dy in ((1, 0), (-1, 0), (0, 1), (0, -1)):
            nx, ny = x + dx, y + dy
            if 0 <= nx < n and 0 <= ny < n and (nx, ny) not in filled:
                g[ny][nx] = OUTLINE
    return ["".join(r) for r in g]


# ── PNG codec (8-bit RGBA, non-interlaced) ──────────────────────────────────


def read_png(path):
    data = open(path, "rb").read()
    assert data[:8] == b"\x89PNG\r\n\x1a\n", path
    pos, idat, w = 8, b"", None
    while pos < len(data):
        (length,) = struct.unpack(">I", data[pos : pos + 4])
        kind = data[pos + 4 : pos + 8]
        body = data[pos + 8 : pos + 8 + length]
        pos += 12 + length
        if kind == b"IHDR":
            w, h, depth, ctype, _, _, interlace = struct.unpack(">IIBBBBB", body)
            assert (depth, ctype, interlace) == (8, 6, 0), f"{path}: need 8-bit RGBA"
        elif kind == b"IDAT":
            idat += body
    raw = zlib.decompress(idat)
    stride, bpp = w * 4, 4
    px = bytearray(stride * h)
    prev = bytearray(stride)
    i = 0
    for y in range(h):
        ft = raw[i]
        line = bytearray(raw[i + 1 : i + 1 + stride])
        i += 1 + stride
        if ft == 1:
            for x in range(bpp, stride):
                line[x] = (line[x] + line[x - bpp]) & 255
        elif ft == 2:
            for x in range(stride):
                line[x] = (line[x] + prev[x]) & 255
        elif ft == 3:
            for x in range(stride):
                a = line[x - bpp] if x >= bpp else 0
                line[x] = (line[x] + ((a + prev[x]) >> 1)) & 255
        elif ft == 4:
            for x in range(stride):
                a = line[x - bpp] if x >= bpp else 0
                b = prev[x]
                c = prev[x - bpp] if x >= bpp else 0
                p = a + b - c
                pa, pb, pc = abs(p - a), abs(p - b), abs(p - c)
                pr = a if pa <= pb and pa <= pc else (b if pb <= pc else c)
                line[x] = (line[x] + pr) & 255
        px[y * stride : (y + 1) * stride] = line
        prev = line
    return w, h, px


def png_bytes(w, h, px):
    stride = w * 4
    rows = []
    prev = bytes(stride)
    for y in range(h):
        line = bytes(px[y * stride : (y + 1) * stride])
        up = bytes((a - b) & 255 for a, b in zip(line, prev))
        rows.append(b"\x02" + up)
        prev = line
    def chunk(kind, body):
        return struct.pack(">I", len(body)) + kind + body + struct.pack(">I", zlib.crc32(kind + body))
    return (
        b"\x89PNG\r\n\x1a\n"
        + chunk(b"IHDR", struct.pack(">IIBBBBB", w, h, 8, 6, 0, 0, 0))
        + chunk(b"IDAT", zlib.compress(b"".join(rows), 9))
        + chunk(b"IEND", b"")
    )


def write_png(path, w, h, px):
    with open(path, "wb") as f:
        f.write(png_bytes(w, h, px))


# ── Geometry and drawing ────────────────────────────────────────────────────


def get(px, w, x, y):
    i = (y * w + x) * 4
    return tuple(px[i : i + 4])


def dist(a, b):
    return sum((p - q) ** 2 for p, q in zip(a[:3], b[:3])) ** 0.5


def is_plate(c):
    # Transparent margin/shadow or the white plate.
    return c[3] < 200 or min(c[:3]) > 225


def scan_in(px, w, coords):
    """Walk `coords` (outside -> centre): past the plate, through the ring,
    and return (index of the first fill pixel, ring colour)."""
    i = 0
    while is_plate(get(px, w, *coords[i])):
        i += 1
    # Settle on the ring's colour a little past its anti-aliased outer edge.
    i = min(i + max(1, len(coords) // 60), len(coords) - 1)
    ring = get(px, w, *coords[i])
    while dist(get(px, w, *coords[i]), ring) < 60 or is_plate(get(px, w, *coords[i])):
        i += 1
    return i, ring


def find_circle(w, h, px):
    mid = h // 2
    left, ring = scan_in(px, w, [(x, mid) for x in range(w // 2)])
    right, _ = scan_in(px, w, [(x, mid) for x in range(w - 1, w // 2, -1)])
    cx = (left + (w - 1 - right)) / 2 + 0.5  # continuous coordinates
    col = int(cx)
    top, _ = scan_in(px, w, [(col, y) for y in range(h // 2)])
    bottom, _ = scan_in(px, w, [(col, y) for y in range(h - 1, h // 2, -1)])
    cy = (top + (h - 1 - bottom)) / 2 + 0.5
    r_in = min(cx - left, (w - right) - cx, cy - top, (h - bottom) - cy)
    inset = max(1, round(r_in * 0.08))
    fill = get(px, w, col, top + inset)
    return cx / w, cy / h, r_in / w, fill


def measure(iconset):
    """The circle, as fractions of the icon's edge, measured on the 1024 px
    master: smaller sizes have one- or two-pixel anti-aliased rings that a
    colour scan can't find reliably, but they share the geometry."""
    w, h, px = read_png(os.path.join(iconset, "icon_512x512@2x.png"))
    return find_circle(w, h, px)


def repaint_fill(w, h, px, cx, cy, r_in, fill):
    r = r_in - 0.75
    for y in range(h):
        for x in range(w):
            if (x + 0.5 - cx) ** 2 + (y + 0.5 - cy) ** 2 <= r * r:
                i = (y * w + x) * 4
                px[i : i + 4] = bytes(fill)


def grid_radius(grid):
    """Farthest filled cell corner from the grid's centre, in cells."""
    n = len(grid)
    c = n / 2
    far = 0.0
    for y, row in enumerate(grid):
        for x, ch in enumerate(row):
            if ch != ".":
                for qx in (x, x + 1):
                    for qy in (y, y + 1):
                        far = max(far, ((qx - c) ** 2 + (qy - c) ** 2) ** 0.5)
    return far


def draw_tiger(w, h, px, cx, cy, r_in, grid16, palette, keyline):
    colours = dict(palette)
    colours[OUTLINE] = palette[keyline]
    # Whole-pixel cells: the biggest that keeps the tiger well inside the ring.
    # At 16 px only the reduced 8x8 tiger fits, and only without the keyline.
    small = reduce_grid(grid16)
    for g in (with_outline(grid16), with_outline(small), small):
        fit = r_in * 0.97 / grid_radius(g)
        cell = int(fit)
        if cell >= 1:
            break
    # Where whole pixels would shrink the tiger by over a quarter (48 px:
    # 1.6 -> 1), use the fractional size: cells snap to pixel edges, so it
    # stays crisp, just with the odd cell a pixel wider.
    if cell / fit < 0.75:
        cell = fit
    n = len(g)
    x0 = cx - n * cell / 2
    y0 = cy - n * cell / 2
    for gy, row in enumerate(g):
        for gx, ch in enumerate(row):
            if ch == ".":
                continue
            rgba = bytes(colours[ch])
            for y in range(round(y0 + gy * cell), round(y0 + (gy + 1) * cell)):
                for x in range(round(x0 + gx * cell), round(x0 + (gx + 1) * cell)):
                    i = (y * w + x) * 4
                    px[i : i + 4] = rgba
    return round(cell, 2), n


def render(path, geo, grid, palette, keyline):
    w, h, px = read_png(path)
    fx, fy, fr, fill = geo
    cx, cy, r_in = fx * w, fy * h, fr * w
    repaint_fill(w, h, px, cx, cy, r_in, fill)
    cell, n = draw_tiger(w, h, px, cx, cy, r_in, grid, palette, keyline)
    return w, h, px, (cx, cy, r_in, cell, n)


def scale_nearest(w, h, px, f):
    out = bytearray(w * f * h * f * 4)
    for y in range(h * f):
        for x in range(w * f):
            j = ((y // f) * w + x // f) * 4
            i = (y * w * f + x) * 4
            out[i : i + 4] = px[j : j + 4]
    return w * f, h * f, out


# ── Containers ──────────────────────────────────────────────────────────────


def ico_bmp_entry(w, h, px):
    """A 32-bit DIB (BGRA, bottom-up) with its AND mask."""
    header = struct.pack("<IiiHHIIiiII", 40, w, h * 2, 1, 32, 0, 0, 0, 0, 0, 0)
    xor = bytearray()
    for y in range(h - 1, -1, -1):
        for x in range(w):
            r, g, b, a = px[(y * w + x) * 4 : (y * w + x) * 4 + 4]
            xor += bytes((b, g, r, a))
    mask_stride = ((w + 31) // 32) * 4
    mask = bytearray(mask_stride * h)
    for row_i, y in enumerate(range(h - 1, -1, -1)):
        for x in range(w):
            if px[(y * w + x) * 4 + 3] == 0:
                mask[row_i * mask_stride + x // 8] |= 0x80 >> (x % 8)
    return header + bytes(xor) + bytes(mask)


def write_ico(path, images):
    """images: [(w, h, px)] ascending; 256 goes in as PNG."""
    blobs = [png_bytes(w, h, px) if w >= 256 else ico_bmp_entry(w, h, px) for w, h, px in images]
    out = struct.pack("<HHH", 0, 1, len(images))
    offset = 6 + 16 * len(images)
    for (w, h, _), blob in zip(images, blobs):
        out += struct.pack("<BBBBHHII", w % 256, h % 256, 0, 0, 1, 32, len(blob), offset)
        offset += len(blob)
    with open(path, "wb") as f:
        f.write(out + b"".join(blobs))


def resized_from_1024(src, size, tmp):
    """The 1024 original resampled by sips (for the 48 px ICO size)."""
    out = os.path.join(tmp, f"base_{size}.png")
    subprocess.run(["sips", "-z", str(size), str(size), src, "--out", out], check=True, capture_output=True)
    return out


def main():
    grid, palette = load_tiger()
    preview = None
    if len(sys.argv) == 3 and sys.argv[1] == "--preview":
        preview = sys.argv[2]
        os.makedirs(preview, exist_ok=True)
    elif len(sys.argv) != 1:
        sys.exit(__doc__)

    for edition, (iconset, icns, ico, keyline) in EDITIONS.items():
        iconset = os.path.join(ROOT, iconset)
        geo = measure(iconset)
        if preview:
            for name, factor in (("icon_256x256@2x.png", 1), ("icon_16x16@2x.png", 8), ("icon_16x16.png", 16)):
                w, h, px, info = render(os.path.join(iconset, name), geo, grid, palette, keyline)
                big = scale_nearest(w, h, px, factor)
                out = os.path.join(preview, f"{edition}_{w}px.png")
                write_png(out, *big)
                print(f"{edition} {name}: circle r={info[2]:.1f} cell={info[3]}px grid={info[4]} -> {out}")
            continue

        rendered = {}
        names = sorted(n for n in os.listdir(iconset) if n.endswith(".png"))
        with tempfile.TemporaryDirectory() as tmp:
            # 48 px is not in the iconset; resample it from the 1024 master.
            # Clear the circle first, so a re-run doesn't resample the old tiger.
            mw, mh, mpx = read_png(os.path.join(iconset, "icon_512x512@2x.png"))
            repaint_fill(mw, mh, mpx, geo[0] * mw, geo[1] * mh, geo[2] * mw, geo[3])
            clean = os.path.join(tmp, "master.png")
            write_png(clean, mw, mh, mpx)
            base48 = resized_from_1024(clean, 48, tmp)
            for name in names:
                w, h, px, info = render(os.path.join(iconset, name), geo, grid, palette, keyline)
                write_png(os.path.join(iconset, name), w, h, px)
                rendered[w] = (w, h, px)
                print(f"{edition} {name}: cell={info[3]}px")
            rendered[48] = render(base48, geo, grid, palette, keyline)[:3]

        subprocess.run(["iconutil", "-c", "icns", iconset, "-o", os.path.join(ROOT, icns)], check=True)
        write_ico(os.path.join(ROOT, ico), [rendered[s] for s in (16, 32, 48, 64, 128, 256)])
        if edition == "standard":
            write_png(os.path.join(ROOT, "assets", "favicon.png"), *rendered[16])
        print(f"{edition}: wrote {icns}, {ico}")
    if not preview and shutil.which("iconutil") is None:
        sys.exit("iconutil not found")


if __name__ == "__main__":
    main()
