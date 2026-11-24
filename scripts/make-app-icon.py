#!/usr/bin/env python3
"""Derive the shipped application icons from the prototype artwork.

Source of truth: ``assets/icons/prototype-icon.png`` — the three-folder/sparkle mark on a flat
white field. This script mattes that field away, crops to the mark, re-lays it out on a square
canvas, and writes every artefact the three desktop platforms want:

  assets/app-icon/zenman-master.png          1024  master, transparent; the gate's pixel source
  assets/app-icon/zenman.png                 256  embedded by the binary (window/dock/taskbar)
  assets/app-icon/zenman.ico              16..256  Windows: the exe resource, Explorer, shortcuts
  assets/app-icon/zenman.icns           16..1024   macOS: ZenMan.app/Contents/Resources
  assets/app-icon/linux/<s>x<s>.png     16..256   Linux: hicolor icon theme, written by
                                                   ``zenman --install-desktop-entry``

``--check`` (what ``cargo xtask app-icon`` runs) verifies the committed artefacts still agree with
the artwork. ``--preview DIR`` dumps a size-ladder montage on light and dark chrome for review.

Deterministic: same input, same pixels. Needs Pillow + numpy, like scripts/make-fixtures.py.
"""

from __future__ import annotations

import argparse
import io
import struct
import sys
from pathlib import Path

import numpy as np
from PIL import Image, ImageFilter

ROOT = Path(__file__).resolve().parent.parent
SOURCE = ROOT / "assets" / "icons" / "prototype-icon.png"
OUT_DIR = ROOT / "assets" / "app-icon"

MASTER_SIZE = 1024
"""Side of the square master canvas."""

FILL = 0.96
"""Fraction of the master canvas the mark's tight box occupies.

The mark is wider than tall, so this fits its long edge and centres the result. The residual
margin is what keeps a 16 px taskbar icon from having its corners touch the frame.
"""

FIELD = np.array((255, 255, 255), dtype=np.float64)
"""The RGB of the field the mark was drawn on."""

FIELD_TOLERANCE = 8.0
"""RGB distance from `FIELD` that the border flood-fill treats as background.

Kept tight on purpose: it is the only step that must not leak into the mark, and it is what
protects field-coloured pixels *inside* the mark (the sparkle, the specular bloom) from going
transparent. Everything beyond it is reached by the two growth steps below.
"""

GLOW_DISTANCE_MAX = 130.0
GLOW_CHROMA_MAX = 60.0
"""How far past the field the background region then grows: pale *and* low-chroma.

The artwork is a sticker: a white halo hugging the mark, and a soft blue-tinted shadow outside
that. Both are invisible on the mark's own field and read as a light blob on a dark taskbar, so
both go. They are also pale and desaturated (shadow chroma ~11-40) while the mark's own outline is
saturated (dark blue border chroma 82, red 110, green 100, light-blue rim 80), so a
chroma-and-distance growth stops on the silhouette instead of eating into the art. Measured: the
growth carves zero pixels of the saturated body.
"""

MEDIAN_KERNEL = 5
"""Median filter on the silhouette mask, which is what makes the cut contour smooth.

The growth boundary follows single-pixel colour noise, so cutting on it alone gives a scalloped
edge once the master is downscaled. The median rounds it off; `blur3` below then feathers it.
"""

RUNTIME_PNG = 256
"""Side of the PNG the binary embeds: enough for a HiDPI taskbar, small enough to ship."""

ICO_SIZES = (16, 24, 32, 48, 64, 128, 256)
"""Windows icon-directory entries; 20/40 are derived by the shell from these."""

LINUX_SIZES = (16, 32, 48, 128, 256)
"""hicolor sizes — the fixed ladder the icon-theme spec names, minus the retina duplicates.
`xtask/src/app_icon.rs` mirrors this tuple, the ICO size list and the ICNS type list; change both."""

# Apple ICNS type codes and the pixel side each one carries, per `iconutil -c icns`.
# The `@2x` codes (`ic11`/`ic12`/`ic13`/`ic14`) are deliberately absent: each one would repeat a
# payload this file already writes (`icp5`/`ic07`/`ic08`/`ic09`) at the same pixel side.
ICNS_TYPES: tuple[tuple[str, int], ...] = (
    ("icp4", 16),    # 16
    ("icp5", 32),    # 32
    ("ic07", 128),   # 128
    ("ic08", 256),   # 256
    ("ic09", 512),   # 512
    ("ic10", 1024),  # 512@2x
)

MASTER = OUT_DIR / "zenman-master.png"
RUNTIME = OUT_DIR / "zenman.png"
ICO = OUT_DIR / "zenman.ico"
ICNS = OUT_DIR / "zenman.icns"


# ------------------------------------------------------------------------------------------- art


def build_master(source: Path) -> Image.Image:
    """The matted, cropped, square-laid-out mark."""
    return square_canvas(matte_from_field(source), MASTER_SIZE, FILL)


def matte_from_field(source: Path) -> Image.Image:
    """RGBA art whose alpha is 0 exactly where the field, the halo and the shadow were.

    The field is flood-filled from the border rather than colour-keyed, so enclosed field-coloured
    pixels (the sparkle, the specular bloom) stay opaque.
    """
    rgb = np.asarray(Image.open(source).convert("RGB"), dtype=np.float64)
    distance = np.sqrt(((rgb - FIELD) ** 2).sum(axis=2))
    chroma = rgb.max(axis=2) - rgb.min(axis=2)

    field = grow_from_border(distance <= FIELD_TOLERANCE)
    outside = grow(field, (distance <= GLOW_DISTANCE_MAX) & (chroma <= GLOW_CHROMA_MAX))

    alpha = blur3(smooth_mask(~outside, MEDIAN_KERNEL))
    alpha[alpha < 0.012] = 0.0  # nothing left of the outer glow, and no colour noise to encode

    # Un-matte: the edge pixels are the mark composited over the field, so recover the colour the
    # compositor would have needed to produce them. Without this the downscale carries a light rim.
    # The divisor is floored, so a near-transparent pixel keeps its own (field-coloured) value
    # instead of an amplified one.
    divisor = np.maximum(alpha, 0.3)[..., None]
    core = (rgb - (1.0 - alpha[..., None]) * FIELD) / divisor
    colour = np.clip(core, 0, 255).round().astype(np.uint8)
    colour[alpha <= 0] = 0
    rgba = np.dstack([colour, (alpha * 255).round().astype(np.uint8)])
    return Image.fromarray(rgba, "RGBA")


def grow_from_border(seedable: np.ndarray) -> np.ndarray:
    """Every pixel reachable from the image border through `seedable` pixels."""
    border = np.zeros_like(seedable)
    border[0, :] = seedable[0, :]
    border[-1, :] = seedable[-1, :]
    border[:, 0] = seedable[:, 0]
    border[:, -1] = seedable[:, -1]
    return grow(border & seedable, seedable)


def grow(region: np.ndarray, seedable: np.ndarray) -> np.ndarray:
    """`region`, extended through every `seedable` pixel touching it (4-connected)."""
    grown = region.copy()
    frontier = adjacent(grown) & seedable
    while frontier.any():
        grown |= frontier
        frontier = adjacent(grown) & seedable & ~grown
    return grown


def adjacent(mask: np.ndarray) -> np.ndarray:
    """One 4-connected dilation, not including `mask` itself."""
    out = np.zeros_like(mask)
    out[1:, :] |= mask[:-1, :]
    out[:-1, :] |= mask[1:, :]
    out[:, 1:] |= mask[:, :-1]
    out[:, :-1] |= mask[:, 1:]
    return out


def smooth_mask(mask: np.ndarray, kernel: int) -> np.ndarray:
    """Median-filter a boolean mask, returning it again as a float 0..1 ramp source."""
    grey = Image.fromarray(np.where(mask, 255, 0).astype(np.uint8), "L")
    return np.asarray(grey.filter(ImageFilter.MedianFilter(kernel)), dtype=np.float64) / 255.0


def blur3(alpha: np.ndarray) -> np.ndarray:
    """One 3x3 box average: the feather a 1024 px master needs before it is downscaled."""
    padded = np.pad(alpha, 1, mode="edge")
    total = padded[:-2, :-2] + padded[1:-1, :-2] + padded[2:, :-2]
    total += padded[:-2, 1:-1] + padded[1:-1, 1:-1] + padded[2:, 1:-1]
    total += padded[:-2, 2:] + padded[1:-1, 2:] + padded[2:, 2:]
    return total / 9.0


def square_canvas(art: Image.Image, side: int, fill: float) -> Image.Image:
    """Scale the mark to `fill` of `side`, then centre it on a transparent canvas."""
    alpha = np.asarray(art)[..., 3]
    ys, xs = np.nonzero(alpha > 12)
    mark = art.crop((int(xs.min()), int(ys.min()), int(xs.max()) + 1, int(ys.max()) + 1))

    scale = side * fill / max(mark.size)
    mark = rescale(mark, (max(1, round(mark.width * scale)), max(1, round(mark.height * scale))))

    canvas = Image.new("RGBA", (side, side), (0, 0, 0, 0))
    canvas.paste(mark, ((side - mark.width) // 2, (side - mark.height) // 2), mark)
    return canvas


def rescale(art: Image.Image, size: tuple[int, int]) -> Image.Image:
    """Lanczos over *premultiplied* alpha, so transparent pixels never darken the edge."""
    if art.size == size:
        return art
    src = np.asarray(art).astype(np.float64)
    alpha = src[..., 3:] / 255.0
    premul = np.dstack([src[..., :3] * alpha, src[..., 3]])
    bands = [
        np.asarray(
            Image.fromarray(premul[..., channel].round().astype(np.uint8), "L").resize(
                size, Image.LANCZOS
            ),
            dtype=np.float64,
        )
        for channel in range(4)
    ]
    out = np.dstack(bands)
    with np.errstate(invalid="ignore", divide="ignore"):
        straight = np.where(out[..., 3:] > 0, out[..., :3] / np.maximum(out[..., 3:] / 255.0, 1e-3), 0.0)
    merged = np.dstack([np.clip(straight, 0, 255), out[..., 3]]).round().astype(np.uint8)
    return Image.fromarray(merged, "RGBA")


# --------------------------------------------------------------------------------------- outputs


def png_bytes(image: Image.Image) -> bytes:
    buffer = io.BytesIO()
    image.save(buffer, format="PNG", optimize=True, compress_level=9)
    return buffer.getvalue()


def display(path: Path) -> str:
    try:
        return str(path.relative_to(ROOT))
    except ValueError:
        return str(path)


def write_icns(master: Image.Image, path: Path) -> None:
    """Hand-written `icns`: a magic, a length, then one PNG payload per icon type.

    `iconutil` is macOS-only and Pillow reads ICNS but cannot write it.
    """
    sides = sorted({side for _, side in ICNS_TYPES})
    payloads = {side: png_bytes(rescale(master, (side, side))) for side in sides}
    body = b"".join(
        name.encode("latin-1") + struct.pack(">I", 8 + len(payloads[side])) + payloads[side]
        for name, side in ICNS_TYPES
    )
    path.write_bytes(b"icns" + struct.pack(">I", 8 + len(body)) + body)


def generate(out_dir: Path, master: Image.Image) -> list[Path]:
    out_dir.mkdir(parents=True, exist_ok=True)
    outputs = [out_dir / name for name in (MASTER.name, RUNTIME.name, ICO.name, ICNS.name)]
    outputs[0].write_bytes(png_bytes(master))
    outputs[1].write_bytes(png_bytes(rescale(master, (RUNTIME_PNG, RUNTIME_PNG))))
    rescale(master, (ICO_SIZES[-1], ICO_SIZES[-1])).save(
        outputs[2], format="ICO", sizes=[(s, s) for s in ICO_SIZES]
    )
    write_icns(master, outputs[3])
    linux = out_dir / "linux"
    linux.mkdir(exist_ok=True)
    for size in LINUX_SIZES:
        target = linux / f"{size}x{size}.png"
        target.write_bytes(png_bytes(rescale(master, (size, size))))
        outputs.append(target)
    return outputs


# ---------------------------------------------------------------------------------------- checks


def check(out_dir: Path, master: Image.Image) -> list[str]:
    """Compare the committed artefacts with freshly-derived pixels; returns the problems.

    The PNGs are compared by *bytes*, because they are what the binary and the icon theme ship and a
    re-encode of a changed master is exactly the drift this catches. The ICO and the ICNS are compared
    structurally instead: which entries they carry and whether the containers are sound, not the encoder
    output inside them, which is free to differ between Pillow versions.
    """
    problems: list[str] = []
    expected = out_dir / MASTER.name
    if not expected.is_file():
        return [f"{display(expected)}: missing — run scripts/make-app-icon.py"]
    committed = Image.open(expected).convert("RGBA")
    if committed.size != master.size:
        problems.append(f"{display(expected)}: {committed.size}, expected {master.size}")
    elif committed.tobytes() != master.tobytes():
        problems.append(
            f"{display(expected)}: pixels differ from {display(SOURCE)} — regenerate after an artwork edit"
        )

    for path, side in [(out_dir / RUNTIME.name, RUNTIME_PNG)] + [
        (out_dir / "linux" / f"{size}x{size}.png", size) for size in LINUX_SIZES
    ]:
        want = png_bytes(rescale(master, (side, side)))
        if not path.is_file():
            problems.append(f"{display(path)}: missing — run scripts/make-app-icon.py")
        elif path.read_bytes() != want:
            problems.append(
                f"{display(path)}: does not match the master — run scripts/make-app-icon.py"
            )

    problems += ico_problems(out_dir / ICO.name)
    problems += icns_problems(out_dir / ICNS.name)
    return problems


def ico_problems(path: Path) -> list[str]:
    if not path.is_file():
        return [f"{display(path)}: missing — run scripts/make-app-icon.py"]
    with Image.open(path) as image:
        found = sorted(image.ico.sizes())
    want = sorted((s, s) for s in ICO_SIZES)
    return [] if found == want else [f"{display(path)}: entries {found}, expected {want}"]


def icns_problems(path: Path) -> list[str]:
    if not path.is_file():
        return [f"{display(path)}: missing — run scripts/make-app-icon.py"]
    blob = path.read_bytes()
    if blob[:4] != b"icns":
        return [f"{display(path)}: not an icns container"]
    got: list[str] = []
    offset = 8
    while offset + 8 <= len(blob):
        got.append(blob[offset : offset + 4].decode("latin-1"))
        offset += max(8, struct.unpack(">I", blob[offset + 4 : offset + 8])[0])
    want = [name for name, _ in ICNS_TYPES]
    return [] if got == want else [f"{display(path)}: icon types {got}, expected {want}"]


def preview(master: Image.Image, out_dir: Path) -> Path:
    """The mark at every shipped side, composited on light chrome and on dark chrome."""
    out_dir.mkdir(parents=True, exist_ok=True)
    sizes = sorted({*ICO_SIZES, *LINUX_SIZES, RUNTIME_PNG})
    tiles = [(size, rescale(master, (size, size))) for size in sizes]
    row_h, gutter = 300, 22
    width = sum(size for size, _ in tiles) + gutter * (len(tiles) + 1)
    sheet = np.zeros((row_h * 2 + gutter * 3, width, 3), dtype=np.uint8)
    for row, chrome in enumerate(((243, 243, 246), (24, 26, 32))):
        top = gutter + row * (row_h + gutter)
        sheet[top : top + row_h, :] = np.array(chrome, dtype=np.uint8)
        x = gutter
        for size, tile in tiles:
            left = x + (size - size) // 2
            top_y = top + row_h - size
            patch = np.asarray(tile).astype(np.float64)
            alpha = patch[..., 3:] / 255.0
            region = sheet[top_y : top_y + size, left : left + size].astype(np.float64)
            sheet[top_y : top_y + size, left : left + size] = (
                patch[..., :3] * alpha + region * (1.0 - alpha)
            ).round().astype(np.uint8)
            x += size + gutter
    out = out_dir / "app-icon-preview.png"
    Image.fromarray(sheet, "RGB").save(out)
    return out


def main(argv: list[str]) -> int:
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("--check", action="store_true", help="verify the committed artefacts only")
    parser.add_argument("--preview", metavar="DIR", help="write a size-ladder montage into DIR")
    parser.add_argument("--out", default=str(OUT_DIR), help="output directory (default assets/app-icon)")
    args = parser.parse_args(argv)

    if not SOURCE.is_file():
        print(f"{SOURCE}: prototype artwork not found", file=sys.stderr)
        return 1

    master = build_master(SOURCE)
    out_dir = Path(args.out).resolve()

    if args.preview:
        print(f"preview {preview(master, Path(args.preview).resolve())}")
    if args.check:
        problems = check(out_dir, master)
        for problem in problems:
            print(f"  {problem}", file=sys.stderr)
        print(f"app-icon: {'FAIL' if problems else 'ok'}")
        return 1 if problems else 0
    for path in generate(out_dir, master):
        print(f"wrote {display(path)} ({path.stat().st_size:,} bytes)")
    return 0


if __name__ == "__main__":
    raise SystemExit(main(sys.argv[1:]))
