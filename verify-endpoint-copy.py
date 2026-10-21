#!/usr/bin/env python3
"""验证：转发接口一键复制（三处 data-copy-endpoint 按钮 + toast + 复制内容含端口全文）"""
import importlib.util, time
spec = importlib.util.spec_from_file_location(
    "cdp_drive", r"C:\Users\Administrator\WorkBuddy\2026-10-14-19-13-31\credit-ferry\cdp-drive.py")
cdp = importlib.util.module_from_spec(spec); spec.loader.exec_module(cdp)
import websocket

ok = True
def check(name, cond, detail=""):
    global ok
    print(("PASS" if cond else "FAIL"), name, str(detail)[:160])
    ok = ok and cond

def wait_page(timeout=40):
    end = time.time() + timeout
    while time.time() < end:
        try: return cdp.find_page()
        except SystemExit: time.sleep(1)
    raise SystemExit("no page target")

ws = websocket.create_connection(wait_page(), timeout=30, suppress_origin=True)
time.sleep(3)

# ① 三处端点复制按钮都已渲染
n = cdp.evaluate(ws, "document.querySelectorAll('[data-copy-endpoint]').length")
check("端点复制按钮=3处", n == 3, n)

# ② hero-endpoint 文案仍随快照填充且未损（span 独立于按钮）
hero_ep = cdp.evaluate(ws, "document.getElementById('hero-endpoint').textContent")
check("hero端点非空", bool(hero_ep) and "6666" in hero_ep, hero_ep)

# ③ view / view2 仍显示完整端点
ev1 = cdp.evaluate(ws, "document.getElementById('endpoint-view').textContent")
ev2 = cdp.evaluate(ws, "document.getElementById('endpoint-view2').textContent")
check("模型区端点含http全文", ev1.startswith("http://") and "/v1" in ev1, ev1)
check("密钥区端点含http全文", ev2.startswith("http://") and "/v1" in ev2, ev2)

# ④ 点击 Hero 按钮：委托命中 → toast「转发接口已复制」
cdp.evaluate(ws, "document.querySelector('#hero-endpoint + .copy-btn, [data-copy-endpoint]').click()")
toast, shown = "", False
for _ in range(10):  # clipboard promise 异步 resolve，1s 内轮询
    time.sleep(0.2)
    toast = cdp.evaluate(ws, "document.getElementById('toast').textContent") or ""
    shown = cdp.evaluate(ws, "document.getElementById('toast').classList.contains('show')")
    if shown and "转发接口" in toast:
        break
check("点击后 toast 提示转发接口已复制", "转发接口已复制" in toast, toast)
check("toast 可见", shown is True, shown)

# ⑤ 复制内容 = latestEndpoint 全文（含 scheme），从闭包不可读则用 toast+剪贴板旁证：
#    注入假 textarea 走 legacy 路径后读 ta 值——execCommand 已被移除无法读；
#    改验 data 源：render 后 window 里 latestEndpoint 通过按钮点击的 copyText 入参间接验证——
#    在页面里劫持 execCommand + clipboard 记录最后一次写入文本。
cap = cdp.evaluate(ws, """(function(){
  window.__cf_cap = null;
  if (navigator.clipboard && navigator.clipboard.writeText) {
    var orig = navigator.clipboard.writeText.bind(navigator.clipboard);
    navigator.clipboard.writeText = function(t){ window.__cf_cap = t; return orig(t); };
  }
  document.querySelectorAll('[data-copy-endpoint]')[2].click();
  return window.__cf_cap;
})()""")
check("劫持捕获复制全文=http端点", cap is not None and str(cap).startswith("http://") and "/v1" in str(cap), cap)
time.sleep(0.3)
toast2 = cdp.evaluate(ws, "document.getElementById('toast').textContent") or ""
check("第三处按钮同样出 toast", "转发接口" in toast2, toast2)

# ⑥ 无 JS 错误（Console 有无异常由 evaluate 失败体现；再跑一次纯渲染轮询）
err = cdp.evaluate(ws, "(window.__cf_err || 'none')")
check("无注入错误", err == "none", err)

print("\n=== ALL PASS ===" if ok else "=== HAS FAIL ===")
raise SystemExit(0 if ok else 1)
