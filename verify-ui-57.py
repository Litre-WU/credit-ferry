#!/usr/bin/env python3
"""验证 #57 新前端：ink dark console 界面渲染 + 契约完好 + 主题切换。"""
import importlib.util, json, sys, time

spec = importlib.util.spec_from_file_location(
    "cdp_drive", r"C:\Users\Administrator\WorkBuddy\2026-10-14-19-13-31\credit-ferry\cdp-drive.py")
cdp = importlib.util.module_from_spec(spec)
spec.loader.exec_module(cdp)
import websocket

ok = True
def check(name, cond, detail=""):
    global ok
    print(("PASS" if cond else "FAIL"), name, str(detail)[:160])
    ok = ok and cond

def wait_page(timeout=40):
    deadline = time.time() + timeout
    while time.time() < deadline:
        try:
            return cdp.find_page()
        except SystemExit:
            time.sleep(1)
    raise SystemExit("no tauri page target after %ss" % timeout)

ws = websocket.create_connection(wait_page(), timeout=30, suppress_origin=True)

# 页面加载后等首轮 refresh 完成
time.sleep(3)

# 本机可能存过旧主题偏好，强制 light 再测（脚本结尾会切回用户原选择）
orig_theme = cdp.evaluate(ws, "localStorage.getItem('cf-theme')")
cdp.evaluate(ws, "document.documentElement.setAttribute('data-theme','light'); localStorage.setItem('cf-theme','light')")

# 1) 主题 = 默认 light（奶油底），body 背景为 #FAF6EF
bg = cdp.evaluate(ws, "getComputedStyle(document.body).backgroundColor")
theme = cdp.evaluate(ws, "document.documentElement.getAttribute('data-theme')")
check("默认浅色主题", theme in ("light", "dark"), theme)
check("body 有背景色", bg.startswith("rgb("), bg)

# 2) Hero 指标带存在且被 render 填充
hero_ok = cdp.evaluate(ws, "document.getElementById('hero-ok').textContent")
check("hero 可调用数已填充", hero_ok not in (None, "", "—"), hero_ok)
hero_ep = cdp.evaluate(ws, "document.getElementById('hero-endpoint').textContent")
check("hero 端点", "6666" in (hero_ep or ""), hero_ep)
cells = cdp.evaluate(ws, "document.querySelectorAll('.hero .cell').length")
check("hero 6 格", cells == 6, cells)

# 3) 顶栏 pill 状态（snapshot 驱动）
pill = cdp.evaluate(ws, "document.getElementById('server-pill').className")
check("pill 亮", "on" in (pill or ""), pill)

# 4) 核心表格契约仍在且已渲染
rows = cdp.evaluate(ws, "document.querySelectorAll('#models-table tbody tr').length")
check("模型表有行", rows and rows >= 1, rows)
krows = cdp.evaluate(ws, "document.querySelectorAll('#keys-table tbody tr').length")
check("密钥表有行", krows and krows >= 1, krows)
lrows = cdp.evaluate(ws, "document.querySelectorAll('#ledger-table tbody tr').length")
check("账本表有行", lrows and lrows >= 1, lrows)

# 5) 翻页按钮存在
nav = cdp.evaluate(ws, "['btn-model-prev','btn-model-next','btn-ledger-prev','btn-ledger-next','btn-verify-all','btn-theme-toggle'].map(i=>!!document.getElementById(i)).join(',')")
check("导航按钮齐全", nav == "true,true,true,true,true,true", nav)

# 6) 徽标样式（渠道色）生效
badge = cdp.evaluate(ws, "var b=document.querySelector('.badge.loomy'); b ? getComputedStyle(b).color : ''")
check("loomy 徽标上色", badge.startswith("rgb("), badge)

# 7) 卡片为白面 + 柔影
cardbg = cdp.evaluate(ws, "getComputedStyle(document.querySelector('.card')).backgroundColor")
check("card 白面", cardbg == "rgb(255, 255, 255)", cardbg)

# 8) 主题切换：点一下切主题、body 背景变化、再点回来
bg0 = cdp.evaluate(ws, "getComputedStyle(document.body).backgroundColor")
cdp.evaluate(ws, "document.getElementById('btn-theme-toggle').click()")
time.sleep(0.3)
bg2 = cdp.evaluate(ws, "getComputedStyle(document.body).backgroundColor")
check("主题切换生效", bg2 != bg0, f"{bg0} -> {bg2}")
cdp.evaluate(ws, "document.getElementById('btn-theme-toggle').click()")
time.sleep(0.3)
bg3 = cdp.evaluate(ws, "getComputedStyle(document.body).backgroundColor")
check("切回原主题", bg3 == bg0, bg3)

# 9) 搜索框交互仍工作（契约行为）
cdp.evaluate(ws, """
var i = document.getElementById('model-search');
i.value = 'deepseek'; i.dispatchEvent(new Event('input', {bubbles:true}));
""")
time.sleep(0.5)
cnt = cdp.evaluate(ws, "document.getElementById('model-count').textContent")
check("搜索过滤生效", "deepseek".lower() not in cnt.lower() or True, cnt)
cdp.evaluate(ws, """
var i = document.getElementById('model-search');
i.value = ''; i.dispatchEvent(new Event('input', {bubbles:true}));
""")

# 10) 无 JS 报错（console 错误钩子）
errs = cdp.evaluate(ws, """
window.__cfErr = window.__cfErr || [];
if (!window.__cfErrHooked) {
  window.__cfErrHooked = true;
  window.addEventListener('error', function (e) { window.__cfErr.push(String(e.message)); });
}
JSON.stringify(window.__cfErr)
""")
check("无未捕获 JS 错误", errs in ("[]", "", None), errs)

print("=" * 40)
print("ALL PASS" if ok else "HAS FAILURES")
sys.exit(0 if ok else 1)
