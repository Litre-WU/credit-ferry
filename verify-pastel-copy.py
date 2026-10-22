#!/usr/bin/env python3
"""验证：① 按钮粉彩底色 ② 模型名称一键复制按钮存在且点击复制。"""
import importlib.util, json, sys, time
spec = importlib.util.spec_from_file_location(
    "cdp_drive", r"C:\Users\Administrator\WorkBuddy\2026-10-14-19-13-31\credit-ferry\cdp-drive.py")
cdp = importlib.util.module_from_spec(spec); spec.loader.exec_module(cdp)
import websocket

ok = True
def check(name, cond, detail=""):
    global ok
    print(("PASS" if cond else "FAIL"), name, str(detail)[:150])
    ok = ok and cond

def wait_page(timeout=40):
    end = time.time() + timeout
    while time.time() < end:
        try: return cdp.find_page()
        except SystemExit: time.sleep(1)
    raise SystemExit("no page target")

ws = websocket.create_connection(wait_page(), timeout=30, suppress_origin=True)
time.sleep(3)

# ① 普通按钮：背景应为粉彩（webview 返回 color(srgb r g b) 或 rgb() 两种格式）
btn_bg = cdp.evaluate(ws, "getComputedStyle(document.querySelector('#btn-loomy-import')).backgroundColor")
check("普通按钮有背景", btn_bg.startswith(("rgb(", "color(")), btn_bg)
def srgb(s):
    if s.startswith("color(srgb"):
        return [round(float(x) * 255) for x in s[s.index("(") + 1:].strip(" )").split()[1:]]
    return [int(x) for x in s.replace("rgb(", "").replace(")", "").split(",")]
try:
    r, g, b = srgb(btn_bg)[:3]
    check("按钮呈粉彩(R>G 红主导)", r > g + 6, f"R{r} G{g} B{b}")
except Exception as e:
    check("按钮粉彩解析", False, e)

# primary 按钮：薄荷渐变（background-image 含 linear-gradient，边框为绿）
pbg = cdp.evaluate(ws, "JSON.stringify([getComputedStyle(document.querySelector('#btn-verify-all')).backgroundImage, getComputedStyle(document.querySelector('#btn-verify-all')).borderColor])")
check("primary 薄荷渐变", "linear-gradient" in pbg, pbg)

# ② 模型名称复制按钮存在
n = cdp.evaluate(ws, "document.querySelectorAll('#models-table [data-copy-model]').length")
check("复制按钮已渲染", n and n >= 1, n)

# 点击第一行复制按钮，读剪贴板（webview 可能无权限，改测 toast 出现 + 按钮属性）
first_name = cdp.evaluate(ws, "document.querySelector('#models-table [data-copy-model]').getAttribute('data-copy-model')")
check("复制按钮带完整客户端id(渠道前缀)", bool(first_name) and " · " in first_name, first_name)
cdp.evaluate(ws, "document.querySelector('#models-table [data-copy-model]').click()")
toast = ""
toast_show = False
for _ in range(10):  # clipboard promise 异步 resolve，轮询 1s 内 toast 出现即可
    time.sleep(0.2)
    toast = cdp.evaluate(ws, "document.getElementById('toast').textContent")
    toast_show = cdp.evaluate(ws, "document.getElementById('toast').classList.contains('show')")
    if toast_show and "复制" in (toast or ""): break
check("点击后 toast 提示", "复制" in (toast or ""), toast)
check("toast 可见", toast_show is True, toast_show)

# 委托只绑一次：再点一次仍工作
cdp.evaluate(ws, "document.querySelector('#models-table [data-copy-model]').click()")
time.sleep(0.3)
check("重复点击仍工作", "复制" in (cdp.evaluate(ws, "document.getElementById('toast').textContent") or ""))

# 无 JS 错误（先装 error 钩子再触发一次重渲染）
cdp.evaluate(ws, """
window.__cfErr = window.__cfErr || [];
if (!window.__cfErrHooked) { window.__cfErrHooked = true;
  window.addEventListener('error', function(e){ window.__cfErr.push(String(e.message)); }); }
""")
time.sleep(3.5)  # 等一轮轮询重渲染
errs = cdp.evaluate(ws, "JSON.stringify(window.__cfErr)")
check("无未捕获错误", errs in ("[]",""), errs)

print("=" * 40)
print("ALL PASS" if ok else "HAS FAILURES")
sys.exit(0 if ok else 1)
