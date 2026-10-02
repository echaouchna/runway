"""Generate web assets and terminal cells from runway-logo.svg (ImageMagick)."""

from pathlib import Path
import subprocess
import xml.etree.ElementTree as ET

ROOT = Path(__file__).resolve().parents[1]
SVG = "http://www.w3.org/2000/svg"
ET.register_namespace("", SVG)
SOURCE = ROOT / "design/runway-logo.svg"


def write_svg(tree, destination):
    ET.indent(tree, space="  ")
    tree.write(destination, encoding="unicode")
    with destination.open("a") as output:
        output.write("\n")


def rasterize(source, destination):
    subprocess.run(
        [
            "magick", "-background", "none", "-density", "288",
            str(source), "-strip", str(destination),
        ],
        check=True,
    )


def main():
    assets = ROOT / "site/assets"
    light = assets / "runway-logo.svg"
    light.write_bytes(SOURCE.read_bytes())
    tree = ET.parse(SOURCE)
    tree.find(f"{{{SVG}}}g[@id='wordmark']").set("fill", "#e7ecff")
    dark = assets / "runway-logo-dark.svg"
    write_svg(tree, dark)

    # Transparent dashes work on both light and dark backgrounds.
    mark = ET.parse(SOURCE)
    mark.getroot().remove(mark.find(f"{{{SVG}}}g[@id='wordmark']"))
    mark.getroot().attrib.update(viewBox="0 0 134 156", width="134", height="156")
    for destination in [assets / "runway-mark.svg", ROOT / "docs/assets/runway-mark.svg"]:
        write_svg(mark, destination)

    mono = ET.parse(assets / "runway-mark.svg")
    mono.find(f"{{{SVG}}}g[@id='mark']").set("fill", "#0b1020")
    write_svg(mono, assets / "runway-mark-mono.svg")

    icon = ET.parse(assets / "runway-mark.svg")
    icon.getroot().attrib.update(viewBox="0 0 192 192", width="192", height="192")
    icon.find(f"{{{SVG}}}g[@id='mark']").attrib.update(
        fill="#ffffff", transform="translate(37 27.36) scale(.88)"
    )
    icon.getroot().insert(1, ET.Element(f"{{{SVG}}}rect", {
        "width": "192", "height": "192", "rx": "40", "fill": "#0b1020",
    }))
    write_svg(icon, assets / "runway-icon.svg")
    write_svg(icon, ROOT / "docs/assets/runway-icon.svg")

    rasterize(light, assets / "runway-logo.png")
    (ROOT / "design/runway-logo.png").write_bytes((assets / "runway-logo.png").read_bytes())
    rasterize(assets / "runway-icon.svg", assets / "runway-icon.png")
    subprocess.run([
        "magick", str(assets / "runway-icon.png"), "-resize", "180x180", "-strip",
        str(assets / "apple-touch-icon.png"),
    ], check=True)

    terminal_cells(dark)


def terminal_cells(dark):
    """Sample the logo into src/branding.txt for the CLI help.

    Each terminal cell shows 2x2 pixels (quadrant block characters). A cell is
    twice as tall as wide, so a pixel covers 3.75 units horizontally and 7.5
    vertically, which keeps the logo's proportions: 562 x 135 units of the
    artwork become 150 x 18 pixels, i.e. 75 x 9 cells. The window is placed so
    that the x-height (y=48) and the baseline (y=108) fall on pixel edges:
    otherwise the 2-unit overshoot of round letters shows up as bumps.
    `-scale` averages exact areas; a pixel is filled when more than half of it
    is covered.
    """
    units = {"x": 12, "y": 10.5, "width": 562, "height": 135}
    width, height = 150, 18
    density = 10  # rendered pixels per artwork unit
    crop = "{w}x{h}+{x}+{y}".format(
        w=units["width"] * density, h=int(units["height"] * density),
        x=units["x"] * density, y=int(units["y"] * density),
    )
    pixels = subprocess.run([
        "magick", "-background", "none", "-density", str(96 * density), str(dark),
        "-crop", crop, "+repage", "-scale", f"{width}x{height}!", "-depth", "8", "rgba:-",
    ], check=True, capture_output=True).stdout
    palette = {"b": (48, 48, 239), "w": (231, 236, 255)}
    rows = []
    for y in range(height):
        row = ""
        for x in range(width):
            offset = (y * width + x) * 4
            red, green, blue, alpha = pixels[offset:offset + 4]
            row += "." if alpha < 128 else min(
                palette,
                key=lambda key: sum((a - b) ** 2 for a, b in zip((red, green, blue), palette[key])),
            )
        rows.append(row)
    rows = equal_dashes(rows, units, width, height)
    (ROOT / "src/branding.txt").write_text("\n".join(rows) + "\n")


def equal_dashes(rows, units, width, height):
    """Redraw the monogram's three road dashes with equal lengths.

    In the artwork they are 22 units long with 11-unit gaps (runway-logo.svg:
    x 34..42, y 46..68, 79..101, 112..134). A 33-unit period is not a whole
    number of 7.5-unit pixels, so sampling yields uneven dashes (3, 3, 2).
    Draw the nearest even pattern instead (3-pixel dashes, 1-pixel gaps),
    centred on the artwork's dashes.
    """
    x_scale = units["width"] / width    # units per pixel horizontally
    y_scale = units["height"] / height  # units per pixel vertically
    dash, gap, count, top, bottom = 22, 11, 3, 46, 134
    columns = [
        x for x in range(width)
        # columns mostly inside the dashes' x range (34..42)
        if 34 <= units["x"] + (x + 0.5) * x_scale <= 42
    ]
    length = max(1, round(dash / y_scale))
    spacing = max(1, round(gap / y_scale))
    total = count * length + (count - 1) * spacing
    centre = ((top + bottom) / 2 - units["y"]) / y_scale
    start = round(centre - total / 2)
    holes = {
        start + i * (length + spacing) + j
        for i in range(count)
        for j in range(length)
    }
    first = int((top - units["y"]) / y_scale) - 1
    last = int((bottom - units["y"]) / y_scale) + 1
    grid = [list(row) for row in rows]
    for y in range(max(first, 0), min(last + 1, height)):
        for x in columns:
            grid[y][x] = "." if y in holes else "b"
    return ["".join(row) for row in grid]


if __name__ == "__main__":
    import sys

    if "--terminal" in sys.argv:
        # Only the CLI help cells, from the committed dark artwork.
        terminal_cells(ROOT / "site/assets/runway-logo-dark.svg")
    else:
        main()
