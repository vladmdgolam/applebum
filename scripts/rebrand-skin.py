#!/usr/bin/env python3
"""Redraws the built-in Winamp skin's title as APPLEBUM, in the skin's own
5-pixel lettering, keeping its green. Also draws the icon.

    python3 scripts/rebrand-skin.py   (needs Pillow; run once, output is committed)
"""
import io, zipfile
from PIL import Image, ImageDraw

SKIN = "assets/skins/spotifast.wsz"
ICON = "packaging/macos/icon-1024.png"

GLYPHS = {
    "A": [".##.", "#..#", "####", "#..#", "#..#"],
    "P": ["###.", "#..#", "###.", "#...", "#..."],
    "L": ["#...", "#...", "#...", "#...", "####"],
    "E": ["####", "#...", "###.", "#...", "####"],
    "B": ["###.", "#..#", "###.", "#..#", "###."],
    "U": ["#..#", "#..#", "#..#", "#..#", ".##."],
    "M": ["#...#", "##.##", "#.#.#", "#...#", "#...#"],
}


def redraw_title(image):
    pixels = image.load()
    # The title's four states, at these rows; x 142-186 holds the old word.
    for top in (4, 19, 61, 76):
        ink = next(pixels[x, top] for x in range(142, 186) if sum(pixels[x, top]) > 200)
        back = pixels[138, top]
        for y in range(top, top + 5):
            for x in range(142, 186):
                pixels[x, y] = back
        x = 146
        for letter in "APPLEBUM":
            glyph = GLYPHS[letter]
            for row, line in enumerate(glyph):
                for column, mark in enumerate(line):
                    if mark == "#":
                        pixels[x + column, top + row] = ink
            x += len(glyph[0]) + 1


def rebuild_skin():
    with zipfile.ZipFile(SKIN) as archive:
        files = {info.filename: archive.read(info.filename) for info in archive.infolist()}
    image = Image.open(io.BytesIO(files["titlebar.bmp"])).convert("RGB")
    redraw_title(image)
    out = io.BytesIO()
    image.save(out, "BMP")
    files["titlebar.bmp"] = out.getvalue()
    with zipfile.ZipFile(SKIN, "w", zipfile.ZIP_DEFLATED) as archive:
        for name, data in files.items():
            archive.writestr(zipfile.ZipInfo(name, (1980, 1, 1, 0, 0, 0)), data)


APPLE = [
    "........##......",
    ".......##.......",
    "...####..####...",
    "..############..",
    ".##HH#########..",
    ".#HH##########..",
    ".#H###########..",
    ".#############..",
    ".#############..",
    ".#############..",
    "..###########...",
    "..###########...",
    "...#########....",
    "....###.###.....",
]


def draw_icon():
    size, cell = 1024, 44
    image = Image.new("RGBA", (size, size), (0, 0, 0, 0))
    draw = ImageDraw.Draw(image)
    draw.rounded_rectangle((100, 100, 924, 924), 186, fill=(15, 17, 20, 255))
    left = (size - cell * 16) // 2 + 22
    top = (size - cell * 14) // 2
    colors = {"#": (250, 45, 72, 255), "H": (255, 150, 165, 255)}
    for y, row in enumerate(APPLE):
        for x, mark in enumerate(row):
            if mark in colors:
                color = (232, 234, 237, 255) if y < 2 else colors[mark]
                draw.rectangle((left + x * cell, top + y * cell, left + (x + 1) * cell - 1, top + (y + 1) * cell - 1), fill=color)
    image.save(ICON)


rebuild_skin()
draw_icon()
