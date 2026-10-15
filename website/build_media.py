#!/usr/bin/env python3
"""Builds the media of the website from a render of the demo video.

    python3 website/build_media.py --video ~/Videos/meerora-demo-3.mp4 --stills WORK/stills

--stills is the stills/ folder of the work folder of docs/video/make-video.sh,
which holds a clean frame (without subtitles) from the middle of every shot.
The script writes into website/site:

- media/meerora-demo.mp4, the video as it is, moved to fast start so it plays
  while it loads
- media/meerora-demo.webm, the same in AV1 and Opus, a fraction of the size
  for browsers that play it (--no-webm skips this slow step)
- media/poster.jpg, the frame the player shows before it plays
- media/stills/*.webp, the pictures of the feature sections
- media/og.png, the picture for links in chats and social networks
- icons/, the logo of assets/logo as favicon, touch icon and for the header
- fonts/, the faces of assets/fonts as WOFF2 with their licences

Needs ffmpeg with libsvtav1 and libopus, Python 3 with Pillow, and fontTools
with brotli for the fonts.
"""

import argparse
import shutil
import subprocess
from pathlib import Path

from PIL import Image, ImageDraw, ImageFilter, ImageFont

HERE = Path(__file__).resolve().parent
REPO = HERE.parent
SITE = HERE / "site"
LOGO = REPO / "assets" / "logo"
FONTS = REPO / "assets" / "fonts"
BG = (0x0B, 0x0D, 0x12)

# The shots whose stills the page shows, by the name the page uses.
STILLS = {
    "space": "stations",
    "needs-you": "needs-you",
    "review": "review-git",
    "external": "external-index",
    "friends": "friends-feed",
    "apps": "desktop-claude",
    "commands": "desktop-commands",
    "rooms": "rooms-station",
}
POSTER = "space-tour"


def ffmpeg(*args: str) -> None:
    subprocess.run(["ffmpeg", "-v", "error", "-y", *args], check=True)


def video(src: Path, webm: bool) -> None:
    media = SITE / "media"
    media.mkdir(parents=True, exist_ok=True)
    ffmpeg("-i", str(src), "-c", "copy", "-movflags", "+faststart", str(media / "meerora-demo.mp4"))
    if webm:
        # SVT-AV1 at preset 6 keeps the small text of the UI sharp at about a third of the size.
        ffmpeg(
            "-i", str(src), "-c:v", "libsvtav1", "-preset", "6", "-crf", "34", "-g", "240", "-pix_fmt", "yuv420p",
            "-c:a", "libopus", "-b:a", "96k", str(media / "meerora-demo.webm"),
        )


def stills(folder: Path) -> None:
    out = SITE / "media" / "stills"
    out.mkdir(parents=True, exist_ok=True)
    for name, shot in STILLS.items():
        image = Image.open(folder / f"{shot}.png").convert("RGB")
        image.resize((1280, 720), Image.LANCZOS).save(out / f"{name}.webp", quality=84, method=6)
    poster = Image.open(folder / f"{POSTER}.png").convert("RGB")
    poster.resize((1920, 1080), Image.LANCZOS).save(SITE / "media" / "poster.jpg", quality=84, optimize=True, progressive=True)


def icons() -> None:
    out = SITE / "icons"
    out.mkdir(parents=True, exist_ok=True)
    logo = Image.open(LOGO / "meerora-logo-1024.png").convert("RGBA")
    for size in (32, 64, 192):
        logo.resize((size, size), Image.LANCZOS).save(out / f"logo-{size}.png", optimize=True)
    shutil.copy(LOGO / "meerora-logo-512.png", out / "logo-512.png")
    # iOS fills transparency with black, so the touch icon gets the page's background and a margin.
    touch = Image.new("RGBA", (180, 180), BG + (255,))
    touch.alpha_composite(logo.resize((136, 136), Image.LANCZOS), (22, 22))
    touch.convert("RGB").save(out / "apple-touch-icon.png", optimize=True)


def og() -> None:
    """1200x630: the logo, the name and the line, as on the end card of the video."""
    card = Image.new("RGBA", (1200, 630), BG + (255,))
    glow = Image.new("RGBA", card.size, (0, 0, 0, 0))
    ImageDraw.Draw(glow).ellipse((330, -120, 870, 400), fill=(0xDA, 0x69, 0x2D, 60))
    card.alpha_composite(glow.filter(ImageFilter.GaussianBlur(110)))
    logo = Image.open(LOGO / "meerora-logo-1024.png").convert("RGBA").resize((220, 220), Image.LANCZOS)
    card.alpha_composite(logo, (490, 92))
    draw = ImageDraw.Draw(card)
    name = ImageFont.truetype(str(FONTS / "bricolage-grotesque-600.ttf"), 84)
    line = ImageFont.truetype(str(FONTS / "bricolage-grotesque-600.ttf"), 40)
    label = ImageFont.truetype(str(FONTS / "ibm-plex-mono-500.ttf"), 22)
    draw.text((600, 368), "meerora", font=name, fill=(0xE8, 0xEA, 0xF0), anchor="mt")
    draw.text((600, 472), "Agents you work with.", font=line, fill=(0xC9, 0xCD, 0xD6), anchor="mt")
    draw.text((600, 548), "PART OF THE MEERVERSE  ·  MEERORA.COM", font=label, fill=(0x9A, 0xA1, 0xB0), anchor="mt")
    card.convert("RGB").save(SITE / "media" / "og.png", optimize=True)


def fonts() -> None:
    from fontTools.ttLib import TTFont

    out = SITE / "fonts"
    out.mkdir(parents=True, exist_ok=True)
    for ttf in sorted(FONTS.glob("*.ttf")):
        font = TTFont(ttf)
        font.flavor = "woff2"
        font.save(out / f"{ttf.stem}.woff2")
    for licence in FONTS.glob("OFL-*.txt"):
        shutil.copy(licence, out / licence.name)


def main() -> None:
    p = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    p.add_argument("--video", type=Path, required=True, help="the rendered demo video (MP4)")
    p.add_argument("--stills", type=Path, required=True, help="the stills/ folder of the video's work folder")
    p.add_argument("--no-webm", action="store_true", help="skip the AV1 version, which takes a few minutes")
    args = p.parse_args()
    stills(args.stills.expanduser())
    icons()
    og()
    fonts()
    video(args.video.expanduser(), not args.no_webm)
    for path in sorted(SITE.rglob("*")):
        if path.is_file() and path.suffix not in (".html", ".css", ".js", ".txt"):
            print(f"{path.stat().st_size / 1e6:8.2f} MB  {path.relative_to(SITE)}")


if __name__ == "__main__":
    main()
