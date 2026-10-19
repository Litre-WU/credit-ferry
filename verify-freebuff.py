#!/usr/bin/env python3
"""Verify the Freebuff upstream over CDP: import -> snapshot -> gated checks."""
import json, os, sys, time, urllib.request

for _k in ("http_proxy", "https_proxy", "HTTP_PROXY", "HTTPS_PROXY",
           "all_proxy", "ALL_PROXY", "no_proxy", "NO_PROXY"):
    os.environ.pop(_k, None)
os.environ["NO_PROXY"] = "*"

import websocket

CDP_LIST = "http://127.0.0.1:9222/json"

def find_page():
    with urllib.request.urlopen(CDP_LIST, timeout=5) as r:
        targets = json.load(r)
    for t in targets:
        if t.get("type") == "page" and "tauri" in t.get("url", ""):
            return t["webSocketDebuggerUrl"]
    raise SystemExit("no tauri page target")

def evaluate(ws, expr, timeout=120):
    ws.send(json.dumps({
        "id": int(time.time() * 1000) % 10**9,
        "method": "Runtime.evaluate",
        "params": {"expression": expr, "awaitPromise": True,
                   "returnByValue": True, "timeout": timeout * 1000},
    }))
    deadline = time.time() + timeout
    while time.time() < deadline:
        msg = json.loads(ws.recv())
        if "id" in msg and msg.get("id") is not None:
            if "result" in msg:
                res = msg["result"].get("result", {})
                if res.get("type") == "string":
                    try:
                        return json.loads(res.get("value"))
                    except Exception:
                        return res.get("value")
                return res.get("value")
            return msg
    raise TimeoutError("CDP evaluate timed out")

def invoke(ws, cmd, args=None):
    expr = ("window.__TAURI_INTERNALS__.invoke(%s, %s || {}, undefined)"
            ".then(v=>JSON.stringify(v)).catch(e=>JSON.stringify({__err:String(e)}))"
            % (json.dumps(cmd), json.dumps(args) if args else "undefined"))
    return evaluate(ws, expr)

ws = websocket.create_connection(find_page(), timeout=30, suppress_origin=True)

print("===== 1. freebuff_import_client =====")
r = invoke(ws, "freebuff_import_client")
print(json.dumps(r, ensure_ascii=False))

print("\n===== 2. get_snapshot.freebuff =====")
snap = invoke(ws, "get_snapshot")
fb = snap.get("freebuff", {})
print(json.dumps({
    "loggedIn": fb.get("loggedIn"),
    "nickname": fb.get("nickname"),
    "gated": fb.get("gated"),
    "quota": fb.get("quota"),
    "modelCount": len(fb.get("models", [])),
    "billing": [m.get("billingStatus") for m in fb.get("models", [])],
    "names": [m.get("name") for m in fb.get("models", [])],
    "priced": [(m.get("name"), m.get("multiplier")) for m in fb.get("models", []) if m.get("multiplier") in (0, 5)],
}, ensure_ascii=False, indent=1))

print("\n===== 3. /v1/models must NOT contain Freebuff =====")
key = open(os.path.join(os.environ["APPDATA"], "CreditFerry", "api-key")).read().strip()
req = urllib.request.Request("http://127.0.0.1:6666/v1/models",
                             headers={"Authorization": "Bearer " + key})
with urllib.request.urlopen(req, timeout=15) as resp:
    models = json.load(resp)
fb_ids = [m["id"] for m in models["data"] if "Freebuff" in m["id"] or "freebuff" in m.get("owned_by", "")]
print("published models:", len(models["data"]), "| Freebuff leaked:", fb_ids)

print("\n===== 4. repeat freebuff_refresh (idempotent) =====")
r2 = invoke(ws, "freebuff_refresh")
print(json.dumps(r2, ensure_ascii=False))

ok = (fb.get("loggedIn") and fb.get("gated") and len(fb.get("models", [])) > 0
      and not fb_ids and (r2.get("models") or 0) > 0)
print("\nRESULT:", "PASS" if ok else "FAIL")
sys.exit(0 if ok else 1)
