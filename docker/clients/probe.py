#!/usr/bin/env python3
"""Reads a screenshot: `probe.py pixel FILE X Y`, `probe.py size FILE`, `probe.py match FILE X Y R G B [TOL]`, `probe.py corner FILE`."""
import sys
from PIL import Image, ImageChops


def block_mean(img, x, y, size=8):
    """Mean color of a small block (the H.264 stream is lossy)."""
    w, h = img.size
    box = img.crop((max(x, 0), max(y, 0), min(x + size, w), min(y + size, h))).convert("RGB")
    px = list(box.getdata())
    return tuple(sum(c[i] for c in px) // len(px) for i in range(3))


def main(argv):
    cmd, path = argv[0], argv[1]
    img = Image.open(path)
    if cmd == "size":
        print(*img.size)
    elif cmd == "pixel":
        print(*block_mean(img, int(argv[2]), int(argv[3])))
    elif cmd == "corner":
        # Bounding box of the magenta corner block of the synthetic desktop: `x0 y0 x1 y1` (last pixel included).
        r, g, b = img.convert("RGB").split()
        mask = ImageChops.multiply(
            ImageChops.multiply(r.point(lambda v: 255 if v > 200 else 0), g.point(lambda v: 255 if v < 70 else 0)),
            b.point(lambda v: 255 if v > 200 else 0),
        )
        box = mask.getbbox()
        if box is None:
            return 1
        print(box[0], box[1], box[2] - 1, box[3] - 1)
    elif cmd == "match":
        x, y = int(argv[2]), int(argv[3])
        want = [int(v) for v in argv[4:7]]
        tol = int(argv[7]) if len(argv) > 7 else 64
        got = block_mean(img, x, y)
        print(*got)
        return 0 if all(abs(g - w) <= tol for g, w in zip(got, want)) else 1
    else:
        return 2
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
