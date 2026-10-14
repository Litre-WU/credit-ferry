"""Generate CreditFerry icon: a little red origami paper boat on water.
Draws a 4096 master, rescales to every existing icon file's exact size,
and rebuilds icon.ico (multi-size) + icon.icns.
"""
import glob, os
from PIL import Image, ImageDraw

W = 4096
cx = W // 2

# palette — cool water background, warm red paper boat
TOP = (198, 235, 246)     # light aqua
BOT = (118, 188, 220)     # water blue
FLAP = (247, 140, 114)    # lightest red (open sides)
SAIL_L = (233, 96, 78)    # sail left face (light)
SAIL_R = (196, 50, 42)    # sail right face (shadow)
HULL = (224, 82, 66)      # main red
HULL_D = (182, 50, 42)    # hull bottom shade
CREAM = (255, 247, 236)   # face
CHEEK = (255, 178, 158)
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
d = ImageDraw.Draw(img)

# ---- boat geometry (4096 space) ----
apex = (cx, 760)
base_y = 2300
sail_bl = (cx - 360, base_y)
sail_br = (cx + 360, base_y)
flap_tip_l = (cx - 1350, 1780)
flap_tip_r = (cx + 1350, 1780)
hull_tl = (cx - 1080, base_y)
hull_tr = (cx + 1080, base_y)
hull_bl = (cx - 820, 2840)
hull_br = (cx + 820, 2840)

# ---- ripples under the boat (floating on the water) ----
d.rounded_rectangle([cx - 470, 2980, cx + 470, 3035], radius=28, fill=WAVE + (150,))
d.rounded_rectangle([cx - 300, 3100, cx + 300, 3150], radius=24, fill=WAVE + (110,))
d.ellipse([cx - 700, 3060, cx - 620, 3130], fill=WAVE + (90,))
d.ellipse([cx + 620, 2940, cx + 700, 3010], fill=WAVE + (90,))

# ---- hull (trapezoid, main red, sits behind the rest of the boat) ----
d.polygon([hull_tl, hull_tr, hull_br, hull_bl], fill=HULL)
# darker bottom band along the waterline
d.polygon([(cx - 812, 2760), (cx + 812, 2760), hull_br, hull_bl], fill=HULL_D)
# folded top rim
d.line([hull_tl, hull_tr], fill=(255, 150, 130), width=10)

# ---- side flaps (the open boat mouth, lightest red) ----
d.polygon([hull_tl, flap_tip_l, sail_bl], fill=FLAP)
d.polygon([hull_tr, flap_tip_r, sail_br], fill=FLAP)

# ---- central sail, two faces (the paper fold cue) ----
d.polygon([apex, sail_bl, (cx, base_y)], fill=SAIL_L)     # light left face
d.polygon([apex, (cx, base_y), sail_br], fill=SAIL_R)     # shaded right face
d.line([apex, (cx, base_y)], fill=HULL_D, width=8)        # center crease

# ---- cute face on the central sail ----
d.ellipse([cx - 165, 1800, cx - 60, 1905], fill=CREAM)    # left eye
d.ellipse([cx + 60, 1800, cx + 165, 1905], fill=CREAM)    # right eye
d.arc([cx - 150, 1980, cx + 150, 2160], start=30, end=150, fill=CREAM, width=26)  # smile
d.ellipse([cx - 260, 1960, cx - 150, 2060], fill=CHEEK + (150,))  # cheeks
d.ellipse([cx + 150, 1960, cx + 260, 2060], fill=CHEEK + (150,))

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
print("done. regenerated", len(pngs), "top-level png + android/ios subdirs + ico + icns")
