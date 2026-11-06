#!/usr/bin/env python3
"""Recolors the built-in Winamp skin from green to apple red and redraws its
title as APPLEBUM, in the skin's own 5-pixel lettering. Also draws the icon.

    python3 scripts/rebrand-skin.py   (needs Pillow; run once, output is committed)
"""
import colorsys, io, re, zipfile
from PIL import Image, ImageDraw

SKIN = "assets/skins/spotifast.wsz"
ICON = "packaging/macos/icon-1024.png"
RED_HUE = 352 / 360

GLYPHS = {
    "A": [".##.", "#..#", "####", "#..#", "#..#"],
    "P": ["###.", "#..#", "###.", "#...", "#..."],
    "L": ["#...", "#...", "#...", "#...", "####"],
    "E": ["####", "#...", "###.", "#...", "####"],
    "B": ["###.", "#..#", "###.", "#..#", "###."],
    "U": ["#..#", "#..#", "#..#", "#..#", ".##."],
    "M": ["#...#", "##.##", "#.#.#", "#...#", "#...#"],
}


def recolor(r, g, b):
    h, s, v = colorsys.rgb_to_hsv(r / 255, g / 255, b / 255)
    if 80 / 360 <= h <= 175 / 360 and s > 0.15:
        r, g, b = colorsys.hsv_to_rgb(RED_HUE, min(1, s * 1.02), v)
        return round(r * 255), round(g * 255), round(b * 255)
    return r, g, b


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
    for name, data in files.items():
        if name.endswith(".bmp"):
            image = Image.open(io.BytesIO(data)).convert("RGB")
            image.putdata([recolor(*pixel) for pixel in image.getdata()])
            if name == "titlebar.bmp":
                redraw_title(image)
            out = io.BytesIO()
            image.save(out, "BMP")
            files[name] = out.getvalue()
        elif name == "viscolor.txt":
            def line(match):
                return "%d,%d,%d" % recolor(*map(int, match.groups()))
            files[name] = re.sub(r"(\d+),(\d+),(\d+)", line, data.decode()).encode()
        elif name == "pledit.txt":
            files[name] = data.decode().replace("#1ED760", "#FA2D48").encode()
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
