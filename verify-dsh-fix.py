#!/usr/bin/env python3
"""Re-verify DSH round-trip on the fixed binary, starting from the 2-block dirty file."""
import json, time, urllib.request
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
                return msg["result"].get("result", {}).get("value")
            return msg
    raise TimeoutError("CDP evaluate timed out")

def invoke(ws, cmd, args=None):
    expr = ("window.__TAURI_INTERNALS__.invoke(%s, %s || {}, undefined)"
            ".then(v=>JSON.stringify(v)).catch(e=>JSON.stringify({__err:String(e)}))"
            % (json.dumps(cmd), json.dumps(args) if args else "undefined"))
    out = evaluate(ws, expr)
    return json.loads(out) if isinstance(out, str) else out

ws = websocket.create_connection(find_page(), timeout=30, suppress_origin=True)

r1 = invoke(ws, "cleanup_dsh")
print("cleanup_dsh ->", json.dumps(r1, ensure_ascii=False))
r2 = invoke(ws, "export_dsh")
print("export_dsh  ->", json.dumps(r2, ensure_ascii=False))
# idempotency: running export again must keep exactly one block
r3 = invoke(ws, "export_dsh")
print("export_dsh#2->", json.dumps(r3, ensure_ascii=False))
ws.close()
