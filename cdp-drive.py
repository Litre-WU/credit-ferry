#!/usr/bin/env python3
"""Drive the CreditFerry panel over CDP: invoke Tauri commands from the webview."""
import json, sys, time, urllib.request
import websocket  # websocket-client

CDP_LIST = "http://127.0.0.1:9222/json"

def find_page():
    with urllib.request.urlopen(CDP_LIST, timeout=5) as r:
        targets = json.load(r)
    for t in targets:
        if t.get("type") == "page" and "tauri" in t.get("url", ""):
            return t["webSocketDebuggerUrl"]
    raise SystemExit("no tauri page target")

def evaluate(ws, expr, await_promise=True, timeout=120):
    ws.send(json.dumps({
        "id": int(time.time() * 1000) % 10**9,
        "method": "Runtime.evaluate",
        "params": {
            "expression": expr,
            "awaitPromise": await_promise,
            "returnByValue": True,
            "timeout": timeout * 1000,
        },
    }))
    deadline = time.time() + timeout
    want = None
    while time.time() < deadline:
        msg = json.loads(ws.recv())
        if "id" in msg and msg.get("id") is not None:
            if "result" in msg:
                res = msg["result"].get("result", {})
                if res.get("type") == "string":
                    return res.get("value")
                return res.get("value")
            return msg
    raise TimeoutError("CDP evaluate timed out")

def invoke(ws, cmd, args=None):
    expr = "window.__TAURI_INTERNALS__.invoke(%s, %s || {}, undefined).then(v=>JSON.stringify(v)).catch(e=>JSON.stringify({__err:String(e)}))" % (
        json.dumps(cmd), json.dumps(args) if args else "undefined"
    )
    out = evaluate(ws, expr)
    if isinstance(out, str):
        return json.loads(out)
    return out

def main():
    try:
        ws = websocket.create_connection(find_page(), timeout=30, suppress_origin=True)
    except Exception:
        ws = websocket.create_connection(find_page(), timeout=30,
                                         header=["Origin: devtools://devtools"])
    action = sys.argv[1] if len(sys.argv) > 1 else "snapshot"
    if action == "snapshot":
        snap = invoke(ws, "get_snapshot")
        md = snap.get("maodao", {})
        print(json.dumps({
            "loggedIn": md.get("loggedIn"),
            "clientInstalled": md.get("clientInstalled"),
            "balance": md.get("balance"),
            "models": [m["clientId"] for m in md.get("models", [])],
            "probe": {m["clientId"]: (m.get("probeOk"), m.get("probeNote")) for m in md.get("models", [])},
        }, ensure_ascii=False, indent=1))
    elif action == "start":
        r = invoke(ws, "maodao_login_start")
        print(json.dumps(r, ensure_ascii=False))
    elif action == "poll":
        deadline = time.time() + (int(sys.argv[2]) if len(sys.argv) > 2 else 300)
        while time.time() < deadline:
            r = invoke(ws, "maodao_login_status")
            st = r.get("status")
            if st == "pending":
                time.sleep(2)
                continue
            print(json.dumps(r, ensure_ascii=False))
            return
        print(json.dumps({"status": "timed_out", "note": "poll window closed"}))
    elif action == "refresh":
        r = invoke(ws, "maodao_refresh")
        print(json.dumps(r, ensure_ascii=False))
    elif action == "probe":
        r = invoke(ws, "probe_models", {"source": "maodao"})
        print(json.dumps(r, ensure_ascii=False))
    elif action == "call":
        ws.close()
        print("use bash curl for chat")
    else:
        print("unknown action")

if __name__ == "__main__":
    main()
