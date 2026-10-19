#!/usr/bin/env python3
"""Verify Task #49/#50/#51 fixes end-to-end:
   - workbuddy chat no longer 11128 (conversation headers + system fold)
   - lobsterai / trae catalogs return real model lists
   - minimax refresh auto-renews instead of 401
Run with the managed python; strips sandbox proxies first.
"""
import json, os, sys, time, urllib.request

for _k in ("http_proxy", "https_proxy", "HTTP_PROXY", "HTTPS_PROXY",
           "all_proxy", "ALL_PROXY", "no_proxy", "NO_PROXY"):
    os.environ.pop(_k, None)
os.environ["NO_PROXY"] = "*"

import websocket

CDP_LIST = "http://127.0.0.1:9222/json"
BASE = "http://127.0.0.1:6666/v1"
KEY = open(os.path.join(os.environ["APPDATA"], "CreditFerry", "api-key")).read().strip()

def find_page():
    with urllib.request.urlopen(CDP_LIST, timeout=5) as r:
        targets = json.load(r)
    for t in targets:
        if t.get("type") == "page" and ("tauri" in t.get("url", "") or "index" in t.get("url", "")):
            return t["webSocketDebuggerUrl"]
    for t in targets:
        if t.get("type") == "page":
            return t["webSocketDebuggerUrl"]
    raise SystemExit("no page target")

def evaluate(ws, expr, timeout=180):
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

def api(path, payload, stream=False):
    req = urllib.request.Request(BASE + path, data=json.dumps(payload).encode(),
                                 headers={"Authorization": "Bearer " + KEY,
                                          "Content-Type": "application/json"},
                                 method="POST")
    try:
        resp = urllib.request.urlopen(req, timeout=120)
        if not stream:
            return resp.status, json.loads(resp.read())
        body = resp.read()
        return resp.status, body[:400]
    except urllib.error.HTTPError as e:
        return e.code, e.read()[:300]

def check(name, cond):
    print(("PASS " if cond else "FAIL ") + name)
    return cond

ok_all = True
ws = websocket.create_connection(find_page(), timeout=30, suppress_origin=True)

print("===== 1. catalog refreshes =====")
for cmd, label in [("lobsterai_refresh", "lobsterai"), ("trae_refresh", "trae"),
                   ("minimax_refresh", "minimax")]:
    r = invoke(ws, cmd)
    print(f"{label} refresh -> {json.dumps(r, ensure_ascii=False)}")
    if label == "minimax":
        ok_all &= check("minimax refresh no 401", isinstance(r, dict) and "401" not in json.dumps(r))
    else:
        ok_all &= check(f"{label} catalog models > 0",
                        isinstance(r, dict) and (r.get("models") or 0) > 0)

print("\n===== 2. pick real published workbuddy model ids =====")
req = urllib.request.Request(BASE + "/models", headers={"Authorization": "Bearer " + KEY})
with urllib.request.urlopen(req, timeout=15) as resp:
    published = json.load(resp)["data"]
wb_ids = [m["id"] for m in published if m.get("owned_by") == "WorkBuddy" or m["id"].startswith("WorkBuddy")]
print("published total:", len(published), "| workbuddy:", wb_ids[:6])
ok_all &= check("workbuddy models published", len(wb_ids) > 0)
# probe-first pick: prefer a cheap one like "WorkBuddy · Hy3" / Balanced
pick = next((i for i in wb_ids if "Hy3" in i), wb_ids[0] if wb_ids else None)
pick2 = next((i for i in wb_ids if "Balanced" in i), wb_ids[1] if len(wb_ids) > 1 else pick)

print("\n===== 3. workbuddy chat via ferry (no leading system) =====")
if pick:
    status, body = api("/chat/completions", {
        "model": pick,
        "messages": [{"role": "user", "content": "Reply with exactly: OK"}],
        "max_tokens": 32,
        "stream": True,
    }, stream=True)
    print(pick, "->", status, str(body)[:160])
    ok_all &= check(f"workbuddy {pick} 200", status == 200)
if pick2:
    status2, body2 = api("/chat/completions", {
        "model": pick2,
        "messages": [{"role": "user", "content": "Reply with exactly: OK"}],
        "max_tokens": 32,
        "stream": False,
    })
    print(pick2, "(non-stream) ->", status2,
          body2 if status2 != 200 else body2.get("choices", [{}])[0].get("message", {}).get("content", "")[:80])
    ok_all &= check(f"workbuddy {pick2} folded 200", status2 == 200)

print("\n===== 4. workbuddy model probes via CDP =====")
r = invoke(ws, "probe_models", {"source": "workbuddy"})
if isinstance(r, dict):
    print("probed:", r.get("probed"), "ok:", r.get("ok"), "failed:", r.get("failed"))
    fails = [v for v in (r.get("verdicts") or []) if not v.get("ok")][:4]
    print("sample fails:", json.dumps(fails, ensure_ascii=False)[:400])
    ok_all &= check("workbuddy probes mostly OK", (r.get("ok") or 0) >= 10)

print("\n===== 4. snapshot after probes =====")
snap = invoke(ws, "get_snapshot")
for src in ("workbuddy", "buddy", "lobsterai", "trae", "minimax"):
    node = snap.get(src, {})
    models = node.get("models", [])
    okc = sum(1 for m in models if m.get("probeOk"))
    print(f"{src}: loggedIn={node.get('loggedIn')} models={len(models)} probeOk={okc}")
    ok_all &= check(f"{src} catalog populated", len(models) > 0)
ok_all &= check("workbuddy probes mostly OK",
                sum(1 for m in snap.get("workbuddy", {}).get("models", []) if m.get("probeOk")) > 10)

print("\nRESULT:", "ALL PASS" if ok_all else "SOME FAILED")
sys.exit(0 if ok_all else 1)
