#!/usr/bin/env python3
"""Generates the Sleepless icon (two bloodshot eyes) in every format the
builds need: icon.png (1024px) / icon-512.png (Linux), icon.ico (Windows)
and icon.icns (macOS).

Only needs Pillow:  python3 assets/make_icon.py
"""
import math
import os
import random

from PIL import Image, ImageChops, ImageDraw, ImageFilter

S = 4096  # draw supersampled, downscale at the end for smooth edges
OUT = os.path.dirname(os.path.abspath(__file__))
rng = random.Random(7)


def almond(cx, cy, w, h, steps=400):
    """Eye shape: two arcs meeting in points at the left/right corners."""
    pts = []
    for i in range(steps + 1):
        t = math.pi * i / steps
        x = cx - w / 2 * math.cos(t)
        pts.append((x, cy - h / 2 * math.sin(t) ** 0.85))
    for i in range(steps + 1):
        t = math.pi * i / steps
        x = cx + w / 2 * math.cos(t)
        pts.append((x, cy + h / 2 * math.sin(t) ** 0.85))
    return pts


def vein(draw, x, y, angle, length, width, depth=0):
    """Wobbly, tapering, branching red line."""
    seg = S * 0.012
    travelled = 0.0
    while travelled < length and width > 1.5:
        angle += rng.uniform(-0.45, 0.45)
        nx = x + math.cos(angle) * seg
        ny = y + math.sin(angle) * seg
        draw.line([(x, y), (nx, ny)], fill=(200, 40, 50, 150), width=int(width))
        draw.ellipse([nx - width / 2, ny - width / 2, nx + width / 2, ny + width / 2],
                     fill=(200, 40, 50, 150))
        x, y = nx, ny
        travelled += seg
        width *= 0.975
        if depth < 3 and rng.random() < 0.08:
            vein(draw, x, y, angle + rng.choice([-1, 1]) * rng.uniform(0.5, 1.1),
                 (length - travelled) * 0.7, width * 0.75, depth + 1)


def eye(img, cx, cy, w, h, look):
    mask = Image.new("L", (S, S), 0)
    ImageDraw.Draw(mask).polygon(almond(cx, cy, w, h), fill=255)

    # Sclera: off-white, flushed pink towards the corners.
    sclera = Image.new("RGBA", (S, S), (250, 244, 238, 255))
    flush = Image.new("L", (S, S), 0)
    fd = ImageDraw.Draw(flush)
    for side in (-1, 1):
        fx = cx + side * w * 0.42
        fd.ellipse([fx - w * 0.28, cy - h * 0.5, fx + w * 0.28, cy + h * 0.5], fill=90)
    flush = flush.filter(ImageFilter.GaussianBlur(S * 0.03))
    sclera = Image.composite(Image.new("RGBA", (S, S), (236, 120, 120, 255)), sclera, flush)

    # Veins creep in from the rim towards the iris.
    veins = Image.new("RGBA", (S, S), (0, 0, 0, 0))
    vd = ImageDraw.Draw(veins)
    for i in range(12):
        side = -1 if i % 2 else 1
        t = rng.uniform(-0.9, 0.9)
        sx = cx + side * w / 2 * rng.uniform(0.55, 1.0)
        sy = cy + t * h * 0.42
        ang = math.atan2(cy - sy, cx - sx) + rng.uniform(-0.35, 0.35)
        vein(vd, sx, sy, ang, w * rng.uniform(0.12, 0.24), S * rng.uniform(0.0025, 0.0045))
    veins = veins.filter(ImageFilter.GaussianBlur(S * 0.0012))
    sclera = Image.alpha_composite(sclera, veins)

    # Iris + pupil + glint.
    d = ImageDraw.Draw(sclera)
    ir = h * 0.36
    ix, iy = cx + look * w * 0.06, cy + h * 0.02
    for k in range(40):  # radial gradient, dark rim to lighter centre
        r = ir * (1 - k / 40)
        c = (int(40 + 60 * k / 40), int(95 + 80 * k / 40), int(60 + 40 * k / 40), 255)
        d.ellipse([ix - r, iy - r, ix + r, iy + r], fill=c)
    pr = ir * 0.42
    d.ellipse([ix - pr, iy - pr, ix + pr, iy + pr], fill=(10, 10, 14, 255))
    gr = ir * 0.18
    gx, gy = ix - ir * 0.35, iy - ir * 0.38
    d.ellipse([gx - gr, gy - gr, gx + gr, gy + gr], fill=(255, 255, 255, 230))

    # Heavy top-lid shadow for that "haven't slept" look.
    shade = Image.new("L", (S, S), 0)
    ImageDraw.Draw(shade).ellipse([cx - w * 0.7, cy - h * 1.25, cx + w * 0.7, cy - h * 0.25], fill=120)
    shade = ImageChops.multiply(shade.filter(ImageFilter.GaussianBlur(S * 0.02)), mask)
    sclera = Image.composite(Image.new("RGBA", (S, S), (60, 20, 30, 255)), sclera, shade)

    # Lid outline + dark under-eye bag.
    bag = Image.new("RGBA", (S, S), (0, 0, 0, 0))
    ImageDraw.Draw(bag).polygon(almond(cx, cy + h * 0.14, w * 1.04, h * 1.15), fill=(70, 30, 60, 170))
    img.alpha_composite(bag.filter(ImageFilter.GaussianBlur(S * 0.012)))
    rim = Image.new("RGBA", (S, S), (0, 0, 0, 0))
    ImageDraw.Draw(rim).polygon(almond(cx, cy, w * 1.03, h * 1.07), fill=(30, 12, 22, 255))
    img.alpha_composite(rim)
    img.paste(sclera, (0, 0), mask)


def tray():
    """Small icons for sleepless-gui: tray.png (just the eyes, square, for
    Windows / Linux trays) and tray-template.png (macOS menu bar: black on
    transparent, macOS tints it to match the bar)."""
    rng.seed(7)
    img = Image.new("RGBA", (S, S), (0, 0, 0, 0))
    w, h = S * 0.38, S * 0.27
    eye(img, S * 0.29, S * 0.5, w, h, look=1)
    eye(img, S * 0.71, S * 0.5, w, h, look=-1)
    eyes = img.crop(img.getbbox())

    ew, eh = eyes.size
    sq = Image.new("RGBA", (ew, ew), (0, 0, 0, 0))
    sq.paste(eyes, (0, (ew - eh) // 2))
    sq.resize((64, 64), Image.LANCZOS).save(os.path.join(OUT, "tray.png"))

    T = 44 * 16  # 22pt @2x, supersampled
    img = Image.new("L", (T, T), 0)
    d = ImageDraw.Draw(img)
    w, h = T * 0.44, T * 0.36
    for cx in (T * 0.26, T * 0.74):
        cy = T * 0.5
        d.polygon(almond(cx, cy, w, h), fill=255)
        d.polygon(almond(cx, cy, w * 0.78, h * 0.62), fill=0)
        r = h * 0.24
        d.ellipse([cx - r, cy - r, cx + r, cy + r], fill=255)
        # Droopy lid across the top of the eye.
        d.rectangle([cx - w / 2, cy - h / 2, cx + w / 2, cy - h * 0.12], fill=255)
        d.polygon(almond(cx, cy, w, h), outline=255)
    keep = Image.new("L", (T, T), 0)
    for cx in (T * 0.26, T * 0.74):
        ImageDraw.Draw(keep).polygon(almond(cx, T * 0.5, w, h), fill=255)
    alpha = ImageChops.multiply(img, keep).resize((44, 44), Image.LANCZOS)
    out = Image.new("RGBA", (44, 44), (0, 0, 0, 0))
    out.putalpha(alpha)
    out.save(os.path.join(OUT, "tray-template.png"))


def main():
    img = Image.new("RGBA", (S, S), (0, 0, 0, 0))
    # macOS-style rounded square, deep night-blue gradient.
    bg = Image.new("RGBA", (S, S), (0, 0, 0, 0))
    bd = ImageDraw.Draw(bg)
    m = S * 0.06
    for y in range(int(m), int(S - m), 8):
        k = (y - m) / (S - 2 * m)
        bd.rectangle([m, y, S - m, y + 8], fill=(int(24 + 30 * k), int(22 + 10 * k), int(58 + 20 * k), 255))
    clip = Image.new("L", (S, S), 0)
    ImageDraw.Draw(clip).rounded_rectangle([m, m, S - m, S - m], radius=S * 0.2, fill=255)
    img.paste(bg, (0, 0), clip)

    w, h = S * 0.38, S * 0.27
    eye(img, S * 0.29, S * 0.5, w, h, look=1)
    eye(img, S * 0.71, S * 0.5, w, h, look=-1)

    big = img.resize((1024, 1024), Image.LANCZOS)
    big.save(os.path.join(OUT, "icon.png"))
    big.resize((512, 512), Image.LANCZOS).save(os.path.join(OUT, "icon-512.png"))
    big.save(os.path.join(OUT, "icon.ico"),
             sizes=[(16, 16), (24, 24), (32, 32), (48, 48), (64, 64), (128, 128), (256, 256)])
    big.save(os.path.join(OUT, "icon.icns"))
    tray()


if __name__ == "__main__":
    main()
