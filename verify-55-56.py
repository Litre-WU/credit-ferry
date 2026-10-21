#!/usr/bin/env python3
"""验证 #55/#56：
1) snapshot 透出 serverRunning / port（前端 pill 一次 IPC 即可）
2) chat 不带 max_tokens 时服务端注入模型 max_output（E2E 无 400/502 回归）
3) 前端 server-pill 文本随快照更新（CDP 读 DOM）
"""
import importlib.util, json, sys, urllib.request

spec = importlib.util.spec_from_file_location(
    "cdp_drive", r"C:\Users\Administrator\WorkBuddy\2026-10-14-19-13-31\credit-ferry\cdp-drive.py")
cdp = importlib.util.module_from_spec(spec)
spec.loader.exec_module(cdp)

DATA = r"C:\Users\Administrator\AppData\Roaming\CreditFerry"
key = open(DATA + r"\api-key", encoding="utf-8").read().strip()

def http_json(url, body=None, headers=None):
    req = urllib.request.Request(url, method="POST" if body is not None else "GET")
    req.add_header("Authorization", "Bearer " + key)
    for k, v in (headers or {}).items():
        req.add_header(k, v)
    data = json.dumps(body).encode() if body is not None else None
    if data:
        req.add_header("Content-Type", "application/json")
    with urllib.request.urlopen(req, data, timeout=120) as r:
        return r.status, r.read()

ok = True
def check(name, cond, detail=""):
    global ok
    print(("PASS" if cond else "FAIL"), name, detail)
    ok = ok and cond

# ---- 1) /v1/models 找一个已发布模型
st, raw = http_json("http://127.0.0.1:6666/v1/models")
models = json.loads(raw)["data"]
check("models 非空", len(models) > 0, str(len(models)))
model_id = models[0]["id"]

# ---- 2) chat 不带 max_tokens：应 200（服务端注入目录 max_output）
st, raw = http_json("http://127.0.0.1:6666/v1/chat/completions", {
    "model": model_id,
    "messages": [{"role": "user", "content": "说一个字"}],
    "stream": False,
})
resp = json.loads(raw) if st == 200 else raw
check("chat 无 max_tokens 200", st == 200, f"model={model_id} status={st}")
if st == 200:
    fr = resp.get("choices", [{}])[0].get("finish_reason")
    check("有 finish_reason", fr is not None, str(fr))

# ---- 3) CDP: snapshot.serverRunning 与前端 pill
import websocket
ws = websocket.create_connection(cdp.find_page(), timeout=30, suppress_origin=True)
snap = cdp.invoke(ws, "get_snapshot")
check("snapshot.serverRunning=true", snap.get("serverRunning") is True)
check("snapshot.port=6666", snap.get("port") == 6666, str(snap.get("port")))

pill = cdp.evaluate(ws, "document.getElementById('server-pill').textContent")
check("pill 文本含端口", "6666" in (pill or ""), repr(pill))
pill_cls = cdp.evaluate(ws, "document.getElementById('server-pill').className")
check("pill class on", "on" in (pill_cls or ""), repr(pill_cls))
ws.close()

print("=" * 40)
print("ALL PASS" if ok else "HAS FAILURES")
sys.exit(0 if ok else 1)
