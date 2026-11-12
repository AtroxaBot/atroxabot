"""Writes atroxabot.ico, the installer's icon, from the app icon's PNGs.

Run it again when crates/desktop/assets/icon/ changes:

    python packaging/windows/icon.py

An .ico is a directory of pictures; Windows takes them PNG-compressed as they are.
"""

import struct
from pathlib import Path

here = Path(__file__).resolve().parent
icons = here.parent.parent / "crates" / "desktop" / "assets" / "icon"
sizes = [16, 20, 24, 32, 40, 48, 64, 256]
pictures = [(icons / f"{size}.png").read_bytes() for size in sizes]

out = struct.pack("<HHH", 0, 1, len(sizes))
offset = 6 + 16 * len(sizes)
for size, picture in zip(sizes, pictures):
    # Width and height (256 is written as 0), no palette, one plane, 32 bits a pixel.
    side = size % 256
    out += struct.pack("<BBBBHHII", side, side, 0, 0, 1, 32, len(picture), offset)
    offset += len(picture)
(here / "atroxabot.ico").write_bytes(out + b"".join(pictures))
