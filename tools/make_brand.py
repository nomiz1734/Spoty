"""Builds the app icon and in-app logo tiles from assets/brand/logo-source.png.

The source is the full logo (mark + wordmark) on an off-white background.
We cut out the mark, turn the background into alpha ("colour to alpha"),
and place it on rounded tiles.
"""
from pathlib import Path

import numpy as np
from PIL import Image, ImageDraw, ImageFilter

ROOT = Path(__file__).resolve().parent.parent
SRC = ROOT / "assets" / "brand" / "logo-source.png"
TILE_BG = (248, 247, 245)


def background_color(img: np.ndarray) -> np.ndarray:
    corners = np.concatenate([img[:40, :40].reshape(-1, 3), img[-40:, -40:].reshape(-1, 3)])
    return np.median(corners, axis=0)


def extract_mark(src: Image.Image) -> Image.Image:
    rgb = np.asarray(src.convert("RGB")).astype(np.float32)
    h, w, _ = rgb.shape
    bg = background_color(rgb)
    # Colour-to-alpha against the background colour.
    diff = np.clip((bg - rgb) / np.maximum(bg, 1), 0, 1)
    alpha = diff.max(axis=2)
    alpha = np.clip((alpha - 0.06) / 0.94, 0, 1)  # drop paper texture
    # The mark is the left part of the logo; stop above the drop shadow.
    region = np.zeros_like(alpha, dtype=bool)
    region[: int(h * 0.64), : int(w * 0.40)] = True
    mask = (alpha > 0.5) & region
    ys, xs = np.nonzero(mask)
    y0, y1, x0, x1 = ys.min(), ys.max() + 1, xs.min(), xs.max() + 1
    pad = 8
    y0, x0 = max(0, y0 - pad), max(0, x0 - pad)
    y1, x1 = min(h, y1 + pad), min(w, x1 + pad)
    a = alpha[y0:y1, x0:x1]
    c = rgb[y0:y1, x0:x1]
    # Un-blend the background so edge pixels keep their true colour.
    safe = np.maximum(a, 1e-3)[..., None]
    col = np.clip((c - bg * (1 - a[..., None])) / safe, 0, 255)
    rgba = np.dstack([col, a * 255]).astype(np.uint8)
    print(f"mark bbox x{x0}-{x1} y{y0}-{y1}")
    return Image.fromarray(rgba, "RGBA")


def rounded_mask(size: int, radius: int, scale: int = 4) -> Image.Image:
    big = Image.new("L", (size * scale, size * scale), 0)
    ImageDraw.Draw(big).rounded_rectangle(
        [0, 0, size * scale - 1, size * scale - 1], radius=radius * scale, fill=255
    )
    return big.resize((size, size), Image.LANCZOS)


def tile(mark: Image.Image, size: int, fill: float, radius_frac: float) -> Image.Image:
    out = Image.new("RGBA", (size, size), TILE_BG + (255,))
    m = mark.copy()
    target = int(size * fill)
    m.thumbnail((target, target), Image.LANCZOS)
    out.alpha_composite(m, ((size - m.width) // 2, (size - m.height) // 2))
    out.putalpha(rounded_mask(size, int(size * radius_frac)))
    return out


def main() -> None:
    mark = extract_mark(Image.open(SRC))
    brand = ROOT / "assets" / "brand"
    mark.save(brand / "logo-mark.png")
    # Launcher icon for the stock OS.
    tile(mark, 300, 0.74, 0.22).save(ROOT / "package" / "stock" / "icon.png", optimize=True)
    # Tiles embedded in the app (login/splash and small headers).
    tile(mark, 168, 0.74, 0.22).save(brand / "tile-168.png", optimize=True)
    tile(mark, 64, 0.76, 0.22).save(brand / "tile-64.png", optimize=True)
    print("done")


if __name__ == "__main__":
    main()
