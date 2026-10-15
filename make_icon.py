"""Generate CreditFerry icon: a little red paper boat sailing to the upper right.
Draws a 4096 master, rescales to every existing icon file's exact size,
and rebuilds icon.ico (multi-size) + icon.icns + the panel header logo.
"""
import glob, os
from PIL import Image, ImageDraw

W = 4096
cx = W // 2

# palette — cool water background, warm red paper boat
TOP = (206, 240, 249)     # light aqua
BOT = (126, 196, 226)     # water blue
FLAP = (250, 150, 126)    # lightest red (folded side wings)
SAIL_L = (236, 100, 82)   # sail left face (light)
SAIL_R = (199, 54, 46)    # sail right face (shadow)
HULL = (226, 84, 68)      # main red
HULL_D = (178, 46, 40)    # hull bottom shade
CREAM = (255, 248, 238)   # face
CHEEK = (255, 176, 156)
WAVE = (255, 255, 255)

# ---- gradient rounded-square background (water) ----
px_img = Image.new("RGBA", (W, W))
px = px_img.load()
for y in range(W):
    t = y / (W - 1)
    r = int(TOP[0] + (BOT[0] - TOP[0]) * t)
    g = int(TOP[1] + (BOT[1] - TOP[1]) * t)
    b = int(TOP[2] + (BOT[2] - TOP[2]) * t)
    for x in range(W):
        px[x, y] = (r, g, b, 255)
mask = Image.new("L", (W, W), 0)
md = ImageDraw.Draw(mask)
md.rounded_rectangle([0, 0, W - 1, W - 1], radius=int(W * 0.22), fill=255)
px_img.putalpha(mask)
img = px_img.convert("RGBA")

# ---- wake: two strokes kept parallel to the heeled waterline ----
TILT = 26          # degrees clockwise: bow points up-right
wake = Image.new("RGBA", (int(W * 0.42), 220), (0, 0, 0, 0))
wd = ImageDraw.Draw(wake)
wd.rounded_rectangle([0, 20, int(W * 0.42), 90], radius=35, fill=WAVE + (150,))
wd.rounded_rectangle([int(W * 0.10), 130, int(W * 0.34), 200], radius=35, fill=WAVE + (100,))
wake = wake.rotate(-TILT + 8, resample=Image.BICUBIC, expand=True)
img.alpha_composite(wake, (int(W * 0.07), int(W * 0.70)))

# ---- boat: drawn upright on its own layer, then tilted toward upper right ----
BOAT = 2500
bc = BOAT // 2
layer = Image.new("RGBA", (BOAT, BOAT), (0, 0, 0, 0))
ld = ImageDraw.Draw(layer)

sail_apex = (bc, 300)
sail_base = 1460
sail_bl = (bc - 300, sail_base)
sail_br = (bc + 300, sail_base)
hull_tl = (bc - 940, sail_base)
hull_tr = (bc + 940, sail_base)
hull_bl = (bc - 660, 2010)
hull_br = (bc + 660, 2010)

# hull (trapezoid) with a darker band at the waterline
ld.polygon([hull_tl, hull_tr, hull_br, hull_bl], fill=HULL)
ld.polygon([(bc - 690, 1880), (bc + 690, 1880), hull_br, hull_bl], fill=HULL_D)
ld.line([hull_tl, hull_tr], fill=(255, 156, 136), width=12)

# folded side wings — the paper-boat cue, kept small and quiet
ld.polygon([hull_tl, (bc - 1180, 1250), (bc - 320, sail_base)], fill=FLAP)
ld.polygon([hull_tr, (bc + 1180, 1250), (bc + 320, sail_base)], fill=FLAP)

# central sail, two faces split by the crease
ld.polygon([sail_apex, sail_bl, (bc, sail_base)], fill=SAIL_L)
ld.polygon([sail_apex, (bc, sail_base), sail_br], fill=SAIL_R)
ld.line([sail_apex, (bc, sail_base)], fill=HULL_D, width=9)

# cute face on the sail
ld.ellipse([bc - 175, 1080, bc - 70, 1185], fill=CREAM)
ld.ellipse([bc + 70, 1080, bc + 175, 1185], fill=CREAM)
ld.arc([bc - 150, 1230, bc + 150, 1400], start=25, end=155, fill=CREAM, width=26)
ld.ellipse([bc - 265, 1215, bc - 165, 1305], fill=CHEEK + (140,))
ld.ellipse([bc + 165, 1215, bc + 265, 1305], fill=CHEEK + (140,))

layer = layer.rotate(-TILT, resample=Image.BICUBIC, expand=True)
# sit the boat a touch left-and-low so the diagonal reads as climbing right
px0 = int(W * 0.50) - layer.width // 2
py0 = int(W * 0.50) - layer.height // 2
img.alpha_composite(layer, (px0, py0))

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
import struct
import io
ico_path = os.path.join(ic_dir, "icon.ico")
sizes = [16, 24, 32, 48, 64, 128, 256]
src = Image.open(os.path.join(ic_dir, "icon-256.png")).convert("RGBA")
blobs = []
for s in sizes:
    buf = io.BytesIO()
    src.resize((s, s), Image.LANCZOS).save(buf, format="PNG")
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

# ---- icon.icns (macOS) ----
icns_path = os.path.join(ic_dir, "icon.icns")
try:
    master.convert("RGB").save(icns_path, format="ICNS")
    print("icns ok")
except Exception as e:
    print("icns skipped:", e)

# ---- preview / reference copies ----
def down(master, size):
    return master.resize((size, size), Image.LANCZOS).convert("RGB")

down(master, 2048).save(ic_dir + "/_master_2048.png")
down(master, 512).save(ic_dir + "/_preview_512.png")
down(master, 128).save(ic_dir + "/_preview_128.png")
down(master, 32).save(ic_dir + "/_preview_32.png")

# ---- the panel header logo lives outside src-tauri/icons ----
root = os.path.dirname(os.path.abspath(__file__))
master.resize((256, 256), Image.LANCZOS).save(os.path.join(root, "src-frontend", "icon.png"))
print("done. regenerated", len(pngs), "top-level png + android/ios subdirs + ico + icns + panel logo")
