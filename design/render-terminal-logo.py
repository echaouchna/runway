"""Regenerate src/branding.txt from the logo (requires Pillow)."""

from pathlib import Path

from PIL import Image

root = Path(__file__).resolve().parents[1]
image = Image.open(root / "design/runway-logo.png").convert("RGBA")
bounds = image.getchannel("A").point(lambda alpha: 255 if alpha > 200 else 0).getbbox()
image = image.crop(bounds).resize((76, 10), Image.Resampling.BOX)
palette = {
    "c": (34, 211, 238),
    "t": (0, 168, 223),
    "b": (0, 143, 212),
    "v": (167, 139, 250),
    "p": (128, 112, 237),
}
rows = []
for y in range(image.height):
    row = ""
    for x in range(image.width):
        red, green, blue, alpha = image.getpixel((x, y))
        source_x = bounds[0] + (x + 0.5) * (bounds[2] - bounds[0]) / image.width
        source_y = bounds[1] + (y + 0.5) * (bounds[3] - bounds[1]) / image.height
        # Match the wordmark region in site/assets/runway-logo-dark.svg.
        if alpha < 110:
            cell = "."
        elif source_x >= 565 and source_y >= 315:
            cell = "w"
        else:
            cell = min(
                palette,
                key=lambda key: sum(
                    (channel - target) ** 2
                    for channel, target in zip((red, green, blue), palette[key])
                ),
            )
        row += cell
    rows.append(row)
(root / "src/branding.txt").write_text("\n".join(rows) + "\n")
