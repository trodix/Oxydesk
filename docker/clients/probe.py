#!/usr/bin/env python3
"""Reads a screenshot: `probe.py pixel FILE X Y`, `probe.py size FILE`, `probe.py match FILE X Y R G B [TOL]`, `probe.py corner FILE`,
`probe.py integrity FILE X Y W H` (exit 0 iff the synthetic background is pixel-exact there),
`probe.py imgdiff FILE_A FILE_B` (max channel difference of two images of the same size)."""
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
    elif cmd == "integrity":
        # The synthetic background is R = v/2+40, G = (v^0x55)/2+40, B = 90 for a v that depends on the pixel and the frame:
        # B == 90 and G == ((R-40) ^ 0x2A) + 40 hold for every pixel of every frame, so a corrupted pixel (lossy
        # codec, wrong stride, byte order, torn rectangle) is caught without knowing the frame. Blocks are skipped.
        x0, y0, w, h = (int(v) for v in argv[2:6])
        rgb = img.convert("RGB")
        bad = total = 0
        for y in range(y0 + 40, y0 + h - 40, 7):
            for x in range(x0 + 120, x0 + w - 40, 5):
                r, g, b = rgb.getpixel((x, y))
                total += 1
                if b != 90 or not 40 <= r <= 167 or g != ((r - 40) ^ 0x2A) + 40:
                    bad += 1
        print(f"{bad}/{total} wrong pixels")
        return 0 if total > 1000 and bad == 0 else 1
    elif cmd == "imgdiff":
        a, b = img.convert("RGB"), Image.open(argv[2]).convert("RGB")
        if a.size != b.size:
            return 1
        print(max(hi for _, hi in ImageChops.difference(a, b).getextrema()))
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
