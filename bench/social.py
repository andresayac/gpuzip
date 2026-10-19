"""The repository's social preview (the picture a link to it shows): docs/img/social-preview.png,
1280x640 as GitHub asks. Its numbers are docs/rescue.md's and docs/benchmarks.md's.

  python bench/social.py
"""

import os

from PIL import Image, ImageDraw, ImageFont

HERE = os.path.dirname(os.path.abspath(__file__))
OUT = os.path.join(HERE, "..", "docs", "img", "social-preview.png")
FONTS = os.path.join(os.environ.get("WINDIR", r"C:\Windows"), "Fonts")
W, H, S = 1280, 640, 2  # drawn at twice the size, then scaled down: smoother edges

BG, PANEL, TEXT, MUTED = (13, 17, 23), (22, 27, 34), (240, 246, 252), (139, 148, 158)
BLUE, GREEN, RED = (42, 120, 214), (46, 160, 67), (218, 54, 51)


def font(name, size):
    return ImageFont.truetype(os.path.join(FONTS, name), size * S)


def main():
    im = Image.new("RGB", (W * S, H * S), BG)
    d = ImageDraw.Draw(im)
    x0 = 64 * S
    d.text((x0, 58 * S), "gpuzip", font=font("CascadiaCode.ttf", 76), fill=TEXT)
    d.text((x0, 160 * S), "Get back what a damaged archive", font=font("segoeuib.ttf", 40), fill=TEXT)
    d.text((x0, 210 * S), "still holds.", font=font("segoeuib.ttf", 40), fill=TEXT)
    body = font("segoeui.ttf", 23)
    lines = [
        "gzip, tar and 7-Zip stop at the first bad byte.",
        "gpuzip rescue keeps going, block by block, and says",
        "which bytes are certain: .gz, .tar.gz, .zip, Office files.",
    ]
    for i, l in enumerate(lines):
        d.text((x0, (292 + 34 * i) * S), l, font=body, fill=MUTED)
    d.text((x0, 418 * S), "And healthy archives fast: a ZIP tests 14-20x faster", font=body, fill=MUTED)
    d.text((x0, 452 * S), "than with 7-Zip; a large .gz decodes on an NVIDIA GPU.", font=body, fill=MUTED)
    d.text((x0, 560 * S), "Windows · Linux · macOS      MIT or Apache-2.0      github.com/andresayac/gpuzip",
           font=font("segoeui.ttf", 20), fill=MUTED)

    # the panel: files back from a tar.gz with three bad disk sectors
    px, py, pw, ph = 792 * S, 150 * S, 424 * S, 330 * S
    d.rounded_rectangle((px, py, px + pw, py + ph), radius=18 * S, fill=PANEL)
    d.text((px + 28 * S, py + 24 * S), "tar.gz of Python's library,", font=font("segoeuib.ttf", 21), fill=TEXT)
    d.text((px + 28 * S, py + 54 * S), "three bad disk sectors", font=font("segoeuib.ttf", 21), fill=TEXT)
    d.text((px + 28 * S, py + 88 * S), "files back, of 1,828", font=font("segoeui.ttf", 18), fill=MUTED)
    bar_x, bar_w = px + 28 * S, pw - 56 * S
    for i, (label, n, color) in enumerate((("the best of gzip, tar, 7-Zip", 87, RED), ("gpuzip rescue", 1722, GREEN))):
        y = py + (138 + 92 * i) * S
        d.text((bar_x, y), label, font=font("segoeui.ttf", 19), fill=TEXT)
        top = y + 30 * S
        d.rounded_rectangle((bar_x, top, bar_x + bar_w, top + 30 * S), radius=6 * S, fill=(48, 54, 61))
        d.rounded_rectangle((bar_x, top, bar_x + max(10 * S, int(bar_w * n / 1828)), top + 30 * S), radius=6 * S, fill=color)
        label_n = f"{n:,}"
        tw = d.textlength(label_n, font=font("segoeuib.ttf", 19))
        inside = bar_w * n / 1828 > tw + 24 * S
        d.text((bar_x + (int(bar_w * n / 1828) - tw - 12 * S if inside else int(bar_w * n / 1828) + 12 * S), top + 3 * S),
               label_n, font=font("segoeuib.ttf", 19), fill=TEXT)
    d.rectangle((0, H * S - 8 * S, W * S, H * S), fill=BLUE)
    im = im.resize((W, H), Image.LANCZOS)
    im.save(OUT, optimize=True)
    print(OUT, os.path.getsize(OUT), "bytes")


if __name__ == "__main__":
    main()
