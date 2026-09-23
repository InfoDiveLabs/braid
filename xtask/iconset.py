#!/usr/bin/env python3
"""Cut a full icon set out of one high-resolution PNG.

Takes a single square-ish source image and writes every size twice: once on an
opaque, optionally rounded tile, and once with a transparent ground for
surfaces that already have a colour of their own.

Three things happen that a plain resize does not:

*Keying.* If the source has no usable alpha, the background colour is detected
from the corners and coverage is recovered from each pixel's distance to it, so
soft edges become real alpha instead of a dark fringe that only looks right on
the background the art was drawn against.

*Un-premultiplying.* An edge pixel is `a*fg + (1-a)*bg`. Reusing that colour
with the recovered alpha would composite the old background a second time and
halo the mark on any other surface, so the foreground colour is solved back out.

*Re-centring.* Source art is rarely centred in its own canvas. The mark's real
bounding box is measured and re-centred with even margins, which also makes the
padding a decision rather than whatever the original file happened to have.

Depends on nothing outside the standard library, so it runs anywhere Python
does: no Pillow, no ImageMagick.

    ./iconset.py logo.png -o assets/brand
    ./iconset.py logo.png -o out --background '#14110E' --sizes 512,256,64
    ./iconset.py logo.png -o out --background none --radius 0
"""

import argparse
import struct
import sys
import zlib
from pathlib import Path

DEFAULT_SIZES = (1024, 512, 256, 128, 64, 32, 16)


# --------------------------------------------------------------------- PNG io
def read_png(path):
    """Decode an 8-bit non-interlaced RGB/RGBA PNG to (width, height, rows)."""
    data = Path(path).read_bytes()
    if data[:8] != b"\x89PNG\r\n\x1a\n":
        raise SystemExit(f"{path}: not a PNG")

    idat, header, pos = bytearray(), None, 8
    while pos < len(data):
        length = struct.unpack(">I", data[pos : pos + 4])[0]
        tag = data[pos + 4 : pos + 8]
        body = data[pos + 8 : pos + 8 + length]
        if tag == b"IHDR":
            header = struct.unpack(">IIBBBBB", body)
        elif tag == b"IDAT":
            idat += body
        elif tag == b"IEND":
            break
        pos += 12 + length

    width, height, depth, colour, _, _, interlace = header
    if depth != 8 or interlace != 0:
        raise SystemExit(f"{path}: need an 8-bit non-interlaced PNG")
    if colour not in (2, 6):
        raise SystemExit(f"{path}: need RGB or RGBA (colour type 2 or 6), got {colour}")
    channels = 3 if colour == 2 else 4

    raw = zlib.decompress(bytes(idat))
    stride = width * channels
    rows, prev, pos = [], bytearray(stride), 0
    for _ in range(height):
        method = raw[pos]
        line = bytearray(raw[pos + 1 : pos + 1 + stride])
        pos += 1 + stride
        # PNG filters are defined against the reconstructed previous scanline.
        for i in range(stride):
            a = line[i - channels] if i >= channels else 0
            b = prev[i]
            c = prev[i - channels] if i >= channels else 0
            if method == 1:
                line[i] = (line[i] + a) & 0xFF
            elif method == 2:
                line[i] = (line[i] + b) & 0xFF
            elif method == 3:
                line[i] = (line[i] + (a + b) // 2) & 0xFF
            elif method == 4:
                p = a + b - c
                pa, pb, pc = abs(p - a), abs(p - b), abs(p - c)
                pred = a if (pa <= pb and pa <= pc) else (b if pb <= pc else c)
                line[i] = (line[i] + pred) & 0xFF
        rows.append(
            [tuple(line[x * channels : (x + 1) * channels]) for x in range(width)]
        )
        prev = line
    return width, height, rows, channels


def write_png(path, size, rows):
    raw = b"".join(bytes([0]) + b"".join(bytes(px) for px in row) for row in rows)

    def chunk(tag, body):
        crc = zlib.crc32(tag + body) & 0xFFFFFFFF
        return struct.pack(">I", len(body)) + tag + body + struct.pack(">I", crc)

    Path(path).write_bytes(
        b"\x89PNG\r\n\x1a\n"
        + chunk(b"IHDR", struct.pack(">IIBBBBB", size, size, 8, 6, 0, 0, 0))
        + chunk(b"IDAT", zlib.compress(raw, 9))
        + chunk(b"IEND", b"")
    )


# ------------------------------------------------------------------ colours
def parse_colour(text):
    text = text.strip().lstrip("#")
    if len(text) == 3:
        text = "".join(c * 2 for c in text)
    if len(text) != 6:
        raise argparse.ArgumentTypeError(f"expected #RRGGBB, got {text!r}")
    return tuple(int(text[i : i + 2], 16) for i in (0, 2, 4))


def detect_background(pixels, width, height):
    """The most common of the four corners, which is what a logo canvas is."""
    corners = [
        pixels[0][0][:3],
        pixels[0][width - 1][:3],
        pixels[height - 1][0][:3],
        pixels[height - 1][width - 1][:3],
    ]
    return max(set(corners), key=corners.count)


# ----------------------------------------------------------------- coverage
def separate(pixels, width, height, channels, background, lo, hi, snap):
    """Coverage in 0..1 plus the un-premultiplied foreground colour."""
    has_alpha = channels == 4 and any(
        px[3] != 255 for row in pixels for px in row[:: max(1, width // 64)]
    )

    cover = [[0.0] * width for _ in range(height)]
    tint = [[(0, 0, 0)] * width for _ in range(height)]

    if has_alpha:
        # The artist already supplied coverage; second-guessing it would only
        # lose the anti-aliasing they drew.
        for y in range(height):
            row, cov, tnt = pixels[y], cover[y], tint[y]
            for x in range(width):
                r, g, b, a = row[x]
                cov[x] = a / 255.0
                tnt[x] = (r, g, b)
        return cover, tint

    br, bg_, bb = background
    # Normalise against the furthest pixel from the background rather than a
    # fixed number, so the thresholds mean the same thing for a pale mark on
    # white as for a bright one on black.
    dmax = 0.0
    for row in pixels:
        for px in row:
            d = (px[0] - br) ** 2 + (px[1] - bg_) ** 2 + (px[2] - bb) ** 2
            dmax = max(dmax, d)
    dmax = dmax**0.5 or 1.0

    for y in range(height):
        row, cov, tnt = pixels[y], cover[y], tint[y]
        for x in range(width):
            r, g, b = row[x][:3]
            d = ((r - br) ** 2 + (g - bg_) ** 2 + (b - bb) ** 2) ** 0.5 / dmax
            a = (d - lo) / (hi - lo)
            a = 0.0 if a <= 0 else (1.0 if a >= 1 else a)
            cov[x] = a
            if a <= 0:
                continue
            if a > 0.15:
                # fg = (pixel - (1 - a) * bg) / a
                tnt[x] = tuple(
                    min(255, max(0, int(round((c - (1 - a) * k) / a))))
                    for c, k in ((r, br), (g, bg_), (b, bb))
                )
            else:
                tnt[x] = (r, g, b)
            if snap:
                tnt[x] = min(
                    snap,
                    key=lambda s, p=tnt[x]: sum((p[i] - s[i]) ** 2 for i in range(3)),
                )
    return cover, tint


# ------------------------------------------------------------------ geometry
def bbox(cover, width, height, threshold=0.5):
    x0, y0, x1, y1 = width, height, -1, -1
    for y in range(height):
        row = cover[y]
        for x in range(width):
            if row[x] >= threshold:
                x0, x1 = min(x0, x), max(x1, x)
                y0, y1 = min(y0, y), max(y1, y)
    if x1 < 0:
        raise SystemExit("no artwork found: the whole image reads as background")
    return x0, y0, x1 + 1, y1 + 1


def frame(cover, tint, box, margin):
    """Centre the artwork in a square canvas with even margins."""
    x0, y0, x1, y1 = box
    art_w, art_h = x1 - x0, y1 - y0
    side = int(max(art_w, art_h) * (1.0 + 2 * margin)) or 1
    ox, oy = (side - art_w) // 2, (side - art_h) // 2

    out_cov = [[0.0] * side for _ in range(side)]
    out_tnt = [[(0, 0, 0)] * side for _ in range(side)]
    for y in range(art_h):
        src_cov, src_tnt = cover[y0 + y], tint[y0 + y]
        dst_cov, dst_tnt = out_cov[oy + y], out_tnt[oy + y]
        for x in range(art_w):
            dst_cov[ox + x] = src_cov[x0 + x]
            dst_tnt[ox + x] = src_tnt[x0 + x]
    return side, out_cov, out_tnt


def resize(cover, tint, source, size):
    """Box-filter down to `size`, weighting colour by coverage."""
    out_cov = [[0.0] * size for _ in range(size)]
    out_tnt = [[(0, 0, 0)] * size for _ in range(size)]
    scale = source / size

    for y in range(size):
        sy0, sy1 = int(y * scale), max(int((y + 1) * scale), int(y * scale) + 1)
        sy1 = min(sy1, source)
        for x in range(size):
            sx0, sx1 = int(x * scale), max(int((x + 1) * scale), int(x * scale) + 1)
            sx1 = min(sx1, source)
            total, acc = 0.0, [0.0, 0.0, 0.0]
            for sy in range(sy0, sy1):
                crow, trow = cover[sy], tint[sy]
                for sx in range(sx0, sx1):
                    a = crow[sx]
                    if a <= 0:
                        continue
                    total += a
                    colour = trow[sx]
                    for i in range(3):
                        acc[i] += colour[i] * a
            count = (sy1 - sy0) * (sx1 - sx0)
            out_cov[y][x] = total / count if count else 0.0
            if total > 0:
                out_tnt[y][x] = tuple(int(round(v / total)) for v in acc)
    return out_cov, out_tnt


def rounded(size, radius):
    """Coverage for a rounded square. `radius` of 0 gives a plain square."""
    if radius <= 0:
        return [[1.0] * size for _ in range(size)]
    mask = [[0.0] * size for _ in range(size)]
    for y in range(size):
        for x in range(size):
            cx = min(max(x + 0.5, radius), size - radius)
            cy = min(max(y + 0.5, radius), size - radius)
            d = ((x + 0.5 - cx) ** 2 + (y + 0.5 - cy) ** 2) ** 0.5
            mask[y][x] = 1.0 if d <= radius - 0.5 else max(0.0, min(1.0, radius + 0.5 - d))
    return mask


def compose(size, cov, tnt, ground, ground_colour):
    """Blend to RGBA rows. `ground` is None for the transparent variant."""
    rows = []
    for y in range(size):
        row = []
        for x in range(size):
            a = cov[y][x]
            if ground is None:
                # Alpha alone carries coverage and the colour stays saturated,
                # so edges composite onto any surface without a fringe.
                row.append((*tnt[y][x], int(round(255 * a))))
                continue
            g = ground[y][x]
            mix = min(a, g)
            px = tuple(
                int(round(ground_colour[i] * (1 - mix) + tnt[y][x][i] * mix))
                for i in range(3)
            )
            row.append((*px, int(round(255 * g))))
        rows.append(row)
    return rows


# ---------------------------------------------------------------------- main
def main(argv=None):
    parser = argparse.ArgumentParser(
        description="Cut a full icon set out of one high-resolution PNG.",
        formatter_class=argparse.RawDescriptionHelpFormatter,
        epilog=__doc__.split("    ./iconset.py")[0].rsplit("\n\n", 1)[-1],
    )
    parser.add_argument("input", type=Path, help="high-resolution source PNG")
    parser.add_argument("-o", "--out", type=Path, default=Path("."), help="output directory")
    parser.add_argument(
        "--sizes",
        default=",".join(str(s) for s in DEFAULT_SIZES),
        help=f"comma-separated pixel sizes (default: {','.join(str(s) for s in DEFAULT_SIZES)})",
    )
    parser.add_argument(
        "--background",
        default="auto",
        help="'auto' to detect from the corners, '#RRGGBB' to force one, "
        "or 'none' to skip the opaque variant entirely (default: auto)",
    )
    parser.add_argument(
        "--radius",
        type=float,
        default=0.225,
        help="corner radius of the opaque tile as a fraction of its size; "
        "0 for a square (default: 0.225)",
    )
    parser.add_argument(
        "--margin",
        type=float,
        default=0.11,
        help="padding around the artwork as a fraction of its longest side (default: 0.11)",
    )
    parser.add_argument(
        "--no-recenter",
        action="store_true",
        help="keep the source framing instead of centring the artwork",
    )
    parser.add_argument(
        "--snap",
        default="",
        help="comma-separated '#RRGGBB' list; every pixel is forced to its "
        "nearest entry, which flattens gradients and compression noise",
    )
    parser.add_argument("--prefix", default="icon", help="basename for the opaque set")
    parser.add_argument(
        "--alpha-prefix", default="mark", help="basename for the transparent set"
    )
    parser.add_argument(
        "--threshold",
        default="0.06,0.22",
        help="lo,hi band of normalised distance-from-background over which a "
        "pixel fades in; ignored when the source already has alpha "
        "(default: 0.06,0.22)",
    )
    args = parser.parse_args(argv)

    sizes = sorted({int(s) for s in args.sizes.split(",") if s.strip()}, reverse=True)
    if not sizes:
        raise SystemExit("--sizes selected nothing")
    lo, hi = (float(v) for v in args.threshold.split(","))
    if not 0 <= lo < hi <= 1:
        raise SystemExit("--threshold must be 0 <= lo < hi <= 1")
    snap = [parse_colour(c) for c in args.snap.split(",") if c.strip()]

    width, height, pixels, channels = read_png(args.input)
    opaque = args.background.lower() != "none"
    if args.background.lower() in ("auto", "none"):
        background = detect_background(pixels, width, height)
    else:
        background = parse_colour(args.background)

    cover, tint = separate(pixels, width, height, channels, background, lo, hi, snap)

    side = max(width, height)
    if not args.no_recenter:
        box = bbox(cover, width, height)
        side, cover, tint = frame(cover, tint, box, args.margin)
        art = f"artwork {box[2] - box[0]}x{box[3] - box[1]} recentred in {side}x{side}"
    else:
        art = "source framing kept"

    print(f"{args.input}: {width}x{height}, background #{'%02X%02X%02X' % background}")
    print(f"  {art}")

    args.out.mkdir(parents=True, exist_ok=True)
    written = []
    for size in sizes:
        cov, tnt = resize(cover, tint, side, size)
        write_png(args.out / f"{args.alpha_prefix}-{size}.png", size, compose(size, cov, tnt, None, background))
        written.append(f"{args.alpha_prefix}-{size}.png")
        if opaque:
            mask = rounded(size, size * args.radius)
            write_png(
                args.out / f"{args.prefix}-{size}.png",
                size,
                compose(size, cov, tnt, mask, background),
            )
            written.append(f"{args.prefix}-{size}.png")

    print(f"  wrote {len(written)} files into {args.out}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
