#!/usr/bin/env python3
"""Task #17 final verification: cleanup_dsh round-trip, export_qoder_intl missing path, export regressions."""
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
                res = msg["result"].get("result", {})
                return res.get("value")
            return msg
    raise TimeoutError("CDP evaluate timed out")

def invoke(ws, cmd, args=None):
    expr = ("window.__TAURI_INTERNALS__.invoke(%s, %s || {}, undefined)"
            ".then(v=>JSON.stringify(v)).catch(e=>JSON.stringify({__err:String(e)}))"
            % (json.dumps(cmd), json.dumps(args) if args else "undefined"))
    out = evaluate(ws, expr)
    return json.loads(out) if isinstance(out, str) else out

ws = websocket.create_connection(find_page(), timeout=30, suppress_origin=True)

steps = [
    ("cleanup_dsh", "cleanup_dsh", None),
    ("export_dsh again", "export_dsh", None),
    ("export_qoder_intl (missing)", "export_qoder_intl", None),
    ("export_qoder CN", "export_qoder", None),
    ("cleanup_qoder CN", "cleanup_qoder", None),
    ("export_qoder CN again", "export_qoder", None),
    ("export_workbuddy", "export_workbuddy", None),
]
for label, cmd, args in steps:
    r = invoke(ws, cmd, args)
    print("== %s -> %s" % (label, json.dumps(r, ensure_ascii=False)))

ws.close()
