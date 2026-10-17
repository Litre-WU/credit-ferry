#!/usr/bin/env python3
"""Verify LobsterAI + TRAE browser-login wiring over CDP."""
import json, sys, time, urllib.request
import websocket

CDP_LIST = "http://127.0.0.1:9222/json"

def find_page():
    with urllib.request.urlopen(CDP_LIST, timeout=5) as r:
        targets = json.load(r)
    for t in targets:
        if t.get("type") == "page" and "tauri" in t.get("url", ""):
            return t["webSocketDebuggerUrl"]
    # fallback: any page
    for t in targets:
        if t.get("type") == "page":
            return t["webSocketDebuggerUrl"]
    raise SystemExit("no page target: " + json.dumps(targets))

def evaluate(ws, expr, await_promise=True, timeout=60):
    ws.send(json.dumps({
        "id": int(time.time() * 1000) % 10**9,
        "method": "Runtime.evaluate",
        "params": {"expression": expr, "awaitPromise": await_promise,
                   "returnByValue": True, "timeout": timeout * 1000},
    }))
    deadline = time.time() + timeout
    while time.time() < deadline:
        msg = json.loads(ws.recv())
        if "id" in msg and msg.get("id") is not None:
            if "result" in msg:
                return msg["result"].get("result", {}).get("value")
            return msg
    raise TimeoutError("evaluate timed out")

def invoke(ws, cmd, args=None):
    expr = ("window.__TAURI_INTERNALS__.invoke(%s, %s || {}, undefined)"
            ".then(v=>JSON.stringify(v)).catch(e=>JSON.stringify({__err:String(e)}))") % (
        json.dumps(cmd), json.dumps(args) if args else "undefined")
    out = evaluate(ws, expr)
    if isinstance(out, str):
        try:
            return json.loads(out)
        except Exception:
            return {"__raw": out}
    return out

def main():
    ws = websocket.create_connection(find_page(), timeout=30, suppress_origin=True)
    report = {}

    # --- backend: invoke the new login commands ---
    r_lob = invoke(ws, "lobsterai_login")
    r_trae = invoke(ws, "trae_login")
    report["lobsterai_login"] = r_lob
    report["trae_login"] = r_trae

    # --- frontend: buttons present ---
    btns = evaluate(ws,
        "JSON.stringify({"
        "  lob: !!document.getElementById('btn-lobsterai-login'),"
        "  trae: !!document.getElementById('btn-trae-login')"
        "})")
    report["buttons_present"] = json.loads(btns) if isinstance(btns, str) else btns

    # --- frontend: simulate click -> login-hint element created ---
    # dispatch click on both buttons, then check for login-hint-<source>
    evaluate(ws,
        "document.getElementById('btn-lobsterai-login').click();"
        "document.getElementById('btn-trae-login').click();",
        await_promise=False)
    time.sleep(2.5)
    hints = evaluate(ws,
        "JSON.stringify({"
        "  lob: !!document.getElementById('login-hint-lobsterai'),"
        "  trae: !!document.getElementById('login-hint-trae'),"
        "  lobHtml: (document.getElementById('login-hint-lobsterai')||{}).innerHTML || '',"
        "  traeHtml: (document.getElementById('login-hint-trae')||{}).innerHTML || ''"
        "})")
    report["hints_after_click"] = json.loads(hints) if isinstance(hints, str) else hints

    print(json.dumps(report, ensure_ascii=False, indent=2))
    ws.close()

if __name__ == "__main__":
    main()
