"""Generate CreditFerry icon: red sailboats sailing from lower left to upper
right, shrinking as they go. Draws a 4096 master, rescales to every existing
icon file's exact size, and rebuilds icon.ico (multi-size) + icon.icns + the
panel header logo.
"""
import glob, os
from PIL import Image, ImageDraw

W = 4096
cx = W // 2

# palette — cool water background, warm red sails and hulls
TOP = (206, 240, 249)     # light aqua
BOT = (126, 196, 226)     # water blue
SAIL_L = (236, 100, 82)   # main sail (light)
SAIL_R = (199, 54, 46)    # jib (shadow side)
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

# ---- one red sailboat, drawn in a 1000x1000 unit box and scaled on demand ----
def sailboat(size: int, with_face: bool) -> Image.Image:
    lay = Image.new("RGBA", (size, size), (0, 0, 0, 0))
    ld = ImageDraw.Draw(lay)
    u = size / 1000.0
    P = lambda pts: [(x * u, y * u) for x, y in pts]
    B = lambda box: [c * u for pt in box for c in pt]
    lw = lambda n: max(2, int(n * u))

    # hull with a darker band at the waterline and a folded rim
    ld.polygon(P([(150, 700), (850, 700), (730, 845), (270, 845)]), fill=HULL)
    ld.polygon(P([(205, 785), (795, 785), (730, 845), (270, 845)]), fill=HULL_D)
    ld.line(P([(150, 700), (850, 700)]), fill=(255, 158, 138), width=lw(10))
    # mast
    ld.line(P([(500, 690), (500, 135)]), fill=HULL_D, width=lw(15))
    # main sail (light, bellied leech) and the shaded jib behind the mast
    ld.polygon(P([(522, 165), (792, 430), (882, 690), (522, 690)]), fill=SAIL_L)
    ld.polygon(P([(478, 250), (478, 668), (268, 668)]), fill=SAIL_R)

    if with_face:
        ld.ellipse(B([(558, 468), (640, 550)]), fill=CREAM)
        ld.ellipse(B([(692, 468), (774, 550)]), fill=CREAM)
        ld.arc(B([(582, 545), (758, 662)]), start=20, end=160, fill=CREAM, width=lw(22))
        ld.ellipse(B([(528, 592), (600, 652)]), fill=CHEEK + (140,))
        ld.ellipse(B([(752, 592), (824, 652)]), fill=CHEEK + (140,))
    return lay


def wake_stroke(x0, x1, y, h, alpha):
    d = ImageDraw.Draw(img)
    d.rounded_rectangle([int(W * x0), int(W * y), int(W * x1), int(W * (y + h))],
                        radius=int(W * h / 2), fill=WAVE + (alpha,))


# ---- fleet: big at the lower left, shrinking toward the upper right ----
TILT = 7  # degrees clockwise, so the boats heel into their course
for fx, fy, size, face in ((0.790, 0.352, 430, False),
                           (0.600, 0.487, 820, False),
                           (0.315, 0.672, 1850, True)):
    wake_stroke(fx - size / W * 0.30, fx + size / W * 0.32, fy + size / W * 0.40,
                size / W * 0.035, 150 if size > 1000 else 105)
    boat = sailboat(size, face).rotate(-TILT, resample=Image.BICUBIC, expand=True)
    img.alpha_composite(boat, (int(W * fx) - boat.width // 2, int(W * fy) - boat.height // 2))

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
