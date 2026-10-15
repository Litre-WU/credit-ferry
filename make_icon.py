"""Generate CreditFerry icon: one red origami paper boat on a transparent
canvas. Draws a 4096 master, rescales to every existing icon file's exact size,
and rebuilds icon.ico (multi-size) + icon.icns + the panel header logo.
"""
import glob, io, os, struct
from PIL import Image, ImageDraw

W = 4096

# palette — the folded-paper reds of the reference shot
PEAK_L = (236, 102, 80)   # the broad near face of the peak
PEAK_R = (205, 65, 53)    # the far face, turned away from the light
HULL = (226, 84, 68)      # main red
HULL_D = (170, 44, 39)    # shaded underside
RIM = (252, 158, 136)     # the folded top edge catching the light


def paper_boat(size: int) -> Image.Image:
    """A classic origami boat, drawn in a 1000x1000 unit box and scaled.

    The right wing rides higher than the left one, so the boat reads as heading
    for the upper-right corner with no rotation applied.
    """
    lay = Image.new("RGBA", (size, size), (0, 0, 0, 0))
    ld = ImageDraw.Draw(lay)
    u = size / 1000.0
    P = lambda pts: [(x * u, y * u) for x, y in pts]
    lw = lambda n: max(2, int(n * u))

    apex, crease_foot = (470, 150), (492, 645)
    # the peak: two faces split by the fold
    ld.polygon(P([apex, (296, 645), crease_foot]), fill=PEAK_L)
    ld.polygon(P([apex, crease_foot, (706, 622)]), fill=PEAK_R)
    ld.line(P([apex, crease_foot]), fill=HULL_D, width=lw(5))

    # hull, drawn over the foot of the peak so the sail grows out of the deck
    ld.polygon(P([(78, 604), (922, 494), (744, 792), (256, 814)]), fill=HULL)
    ld.polygon(P([(218, 750), (790, 730), (744, 792), (256, 814)]), fill=HULL_D)
    ld.line(P([(78, 604), (922, 494)]), fill=RIM, width=lw(11))
    return lay


img = Image.new("RGBA", (W, W), (0, 0, 0, 0))
boat = paper_boat(3600)
img.alpha_composite(boat, ((W - boat.width) // 2, (W - boat.height) // 2))
master = img

ic_dir = os.path.dirname(os.path.abspath(__file__)) + "/src-tauri/icons"


def save_size(path, size):
    master.resize((size, size), Image.LANCZOS).save(path)


pngs = sorted(glob.glob(ic_dir + "/*.png"))
for f in pngs:
    save_size(f, Image.open(f).size[0])
for sub in ("android", "ios"):
    for f in sorted(glob.glob(os.path.join(ic_dir, sub, "*.png"))):
        save_size(f, Image.open(f).size[0])

# ---- icon.ico (multi-size, PNG-compressed frames) ----
# PIL's append= is unreliable (often emits a single frame), so build the
# ICO container by hand: header + directory + one PNG blob per size.
ico_path = os.path.join(ic_dir, "icon.ico")
sizes = [16, 24, 32, 48, 64, 128, 256]
blobs = []
for s in sizes:
    buf = io.BytesIO()
    master.resize((s, s), Image.LANCZOS).save(buf, format="PNG")
    blobs.append(buf.getvalue())
out = io.BytesIO()
out.write(struct.pack("<HHH", 0, 1, len(blobs)))
offset = 6 + len(blobs) * 16
directory = b""
for s, blob in zip(sizes, blobs):
    w = 0 if s >= 256 else s
    directory += struct.pack("<BBBBHHII", w, w, 0, 0, 1, 32, len(blob), offset)
    offset += len(blob)
out.write(directory)
for blob in blobs:
    out.write(blob)
with open(ico_path, "wb") as fh:
    fh.write(out.getvalue())

# ---- icon.icns (macOS) — alpha has to survive, so no RGB conversion ----
icns_path = os.path.join(ic_dir, "icon.icns")
try:
    master.save(icns_path, format="ICNS")
    print("icns ok")
except Exception as e:
    print("icns skipped:", e)

# ---- preview copies, left RGBA so the transparency is visible ----
for size in (2048, 512, 128, 32):
    name = "_master_2048.png" if size == 2048 else "_preview_%d.png" % size
    master.resize((size, size), Image.LANCZOS).save(os.path.join(ic_dir, name))

# ---- the panel header logo lives outside src-tauri/icons ----
root = os.path.dirname(os.path.abspath(__file__))
master.resize((256, 256), Image.LANCZOS).save(os.path.join(root, "src-frontend", "icon.png"))
print("done. regenerated", len(pngs), "top-level png + android/ios subdirs + ico + icns + panel logo")
