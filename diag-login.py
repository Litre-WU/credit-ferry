#!/usr/bin/env python3
"""Diagnose channel login state: snapshot fields + login command returns."""
import json, os, time, urllib.request
import websocket

# The sandbox exports a global http_proxy pointing at a local intercepting
# proxy. Loopback traffic must NOT be routed through it, so strip it.
for _k in ("http_proxy", "https_proxy", "HTTP_PROXY", "HTTPS_PROXY",
           "all_proxy", "ALL_PROXY", "no_proxy", "NO_PROXY"):
    os.environ.pop(_k, None)
os.environ["no_proxy"] = "*"
os.environ["NO_PROXY"] = "*"
urllib.request.getproxies = lambda: {}

CDP_LIST = "http://127.0.0.1:9222/json"

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

def evaluate(ws, expr, await_promise=True, timeout=90):
    ws.send(json.dumps({
        "id": int(time.time() * 1000) % 10**9,
        "method": "Runtime.evaluate",
        "params": {"expression": expr, "awaitPromise": await_promise,
                   "returnByValue": True, "timeout": timeout * 1000},
    }))
    deadline = time.time() + timeout
    while time.time() < deadline:
        msg = json.loads(ws.recv())
        if msg.get("id") is not None:
            if "result" in msg:
                return msg["result"].get("result", {}).get("value")
            return msg
    raise TimeoutError("evaluate timed out")

def invoke(ws, cmd, args=None):
    expr = ("window.__TAURI_INTERNALS__.invoke(%s, %s || {}, undefined)"
            ".then(v=>JSON.stringify(v)).catch(e=>JSON.stringify({__err:String(e)}))") % (
        json.dumps(cmd), json.dumps(args) if args is not None else "undefined")
    out = evaluate(ws, expr)
    if isinstance(out, str):
        try:
            return json.loads(out)
        except Exception:
            return {"__raw": out}
    return out

SOURCES = ["qodercn", "minimax", "cline", "lobsterai", "trae",
           "buddy", "workbuddy", "loomy", "raccoon", "maodao", "zen", "kilo", "ofm"]

def main():
    ws = websocket.create_connection(find_page(), timeout=30, suppress_origin=True)
    snap = invoke(ws, "get_snapshot")
    print("=== SNAPSHOT KEYS ===")
    if isinstance(snap, dict):
        print(list(snap.keys()))
        for s in SOURCES:
            blk = snap.get(s)
            if blk is None:
                print(f"  {s}: <absent>")
                continue
            if isinstance(blk, dict):
                keep = {k: blk.get(k) for k in
                        ("loggedIn", "models", "uid", "nickname", "balance",
                         "note", "gateNote", "gated", "online", "lastError")}
                print(f"  {s}: {json.dumps(keep, ensure_ascii=False)}")
            else:
                print(f"  {s}: {blk!r}")

    print()
    print("=== LOGIN COMMANDS ===")
    for s in SOURCES:
        r = invoke(ws, f"{s}_login")
        print(f"  {s}_login -> {json.dumps(r, ensure_ascii=False)}")

    print()
    print("=== FRONTEND CARD CHECK ===")
    fe = evaluate(ws, """(() => {
        const ids = ['buddy','workbuddy','lobsterai','trae','minimax','cline','qoder','qodercn','loomy','raccoon','maodao','zen','kilo'];
        const out = {};
        for (const id of ids) {
            // Cards have no stable id; locate via any button then climb to .card
            let anchor = document.getElementById('btn-' + id + '-login')
                      || document.getElementById('btn-' + id + '-refresh')
                      || document.getElementById('btn-' + id + '-probe');
            let card = null;
            if (anchor) { let n = anchor; while (n && !(n.classList && n.classList.contains('card'))) n = n.parentElement; card = n; }
            out[id] = {
                loginBtn: !!document.getElementById('btn-' + id + '-login'),
                refreshBtn: !!document.getElementById('btn-' + id + '-refresh'),
                probeBtn: !!document.getElementById('btn-' + id + '-probe'),
                balanceStat: !!document.getElementById(id + '-balance'),
                modelsStat: !!document.getElementById(id + '-models'),
                statusEl: !!document.getElementById(id + '-status'),
                h: card ? Math.round(card.getBoundingClientRect().height) : null,
                w: card ? Math.round(card.getBoundingClientRect().width) : null,
                overflow: card ? (card.scrollWidth > card.clientWidth + 2) : null,
            };
        }
        return JSON.stringify(out);
    })()""")
    try:
        print(json.dumps(json.loads(fe), ensure_ascii=False, indent=2))
    except Exception:
        print(fe)
    ws.close()

if __name__ == "__main__":
    main()
