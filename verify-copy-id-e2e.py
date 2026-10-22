#!/usr/bin/env python3
"""端到端验证：面板复制按钮读出的文本，直接 POST /v1/chat/completions 必须能被路由。
用户报障：复制的是纯显示名（如 DeepSeek V4.1 Flash），粘进 WorkBuddy 后 400「无法连接到模型服务」。
修复：复制按钮改为携带完整客户端 id（Loomy · xxx），与转发口 resolve_model 认可的格式一致。"""
import importlib.util, json, time, urllib.request
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

# ① 按钮携带的文本 = 完整客户端 id（含 " · " 渠道前缀）
copied = cdp.evaluate(ws, "document.querySelector('#models-table [data-copy-model]').getAttribute('data-copy-model')")
check("复制文本含渠道前缀", bool(copied) and " · " in copied, copied)

# ② 劫持 clipboard.writeText 捕获真实写入内容 == 按钮属性
cap = cdp.evaluate(ws, """(function(){
  window.__cf_cap = null;
  var orig = navigator.clipboard.writeText.bind(navigator.clipboard);
  navigator.clipboard.writeText = function(t){ window.__cf_cap = t; return orig(t); };
  document.querySelector('#models-table [data-copy-model]').click();
  return window.__cf_cap;
})()""")
check("捕获复制内容=完整id", cap == copied, cap)

# ③ 读取主密钥（直接读 %APPDATA%\CreditFerry\api-key，页面闭包里的 invoke 不可达）
import os
kp = os.path.expandvars(r"%APPDATA%\CreditFerry\api-key")
key = open(kp, encoding="utf-8").read().strip()
check("主密钥已取", bool(key) and len(key) > 8, key[:10] + "…")

# ④ 用复制到的这个 id 真打一发 chat/completions —— 必须能被路由（非 400 Select model）
body = json.dumps({"model": copied, "messages": [{"role": "user", "content": "回复OK"}], "max_tokens": 8}).encode()
req = urllib.request.Request("http://127.0.0.1:6666/v1/chat/completions", data=body,
    headers={"Authorization": "Bearer " + key, "Content-Type": "application/json"})
try:
    with urllib.request.urlopen(req, timeout=120) as r:
        data = json.loads(r.read())
        check("复制id直连chat成功", "choices" in data, data.get("model"))
except urllib.error.HTTPError as e:
    msg = e.read().decode(errors="replace")[:200]
    check("复制id直连chat成功", False, f"{e.code} {msg}")

# ⑤ 反证：旧行为（纯显示名，去掉渠道前缀）必须 400 —— 证明修复确实针对根因
bare = copied.split(" · ", 1)[1] if " · " in copied else copied
req2 = urllib.request.Request("http://127.0.0.1:6666/v1/chat/completions",
    data=json.dumps({"model": bare, "messages": [{"role": "user", "content": "hi"}], "max_tokens": 8}).encode(),
    headers={"Authorization": "Bearer " + key, "Content-Type": "application/json"})
try:
    with urllib.request.urlopen(req2, timeout=30) as r:
        check("纯显示名应被拒(反证)", False, "竟然 200：" + bare)
except urllib.error.HTTPError as e:
    check("纯显示名被拒(反证)", e.code == 400, f"{e.code} {bare}")

print("\n=== ALL PASS ===" if ok else "=== HAS FAIL ===")
raise SystemExit(0 if ok else 1)
