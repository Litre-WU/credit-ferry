#!/usr/bin/env python3
"""End-to-end check for the Loomy WeChat QR login flow.

Invokes `loomy_wechat_login` over CDP, then fetches the returned local
QR page and verifies it actually embeds a WeChat QR image (data URL).
"""
import json
import os
import re
import time
import urllib.request

import websocket

# The sandbox exports a global http_proxy pointing at a local intercepting
# proxy. Loopback traffic must NOT be routed through it, so strip it.
for _k in ("http_proxy", "https_proxy", "HTTP_PROXY", "HTTPS_PROXY",
           "all_proxy", "ALL_PROXY", "no_proxy", "NO_PROXY"):
    os.environ.pop(_k, None)
os.environ["no_proxy"] = "*"
os.environ["NO_PROXY"] = "*"
urllib.request.getproxies = lambda: {}


def find_page():
    with urllib.request.urlopen("http://127.0.0.1:9222/json", timeout=5) as r:
        targets = json.load(r)
    for t in targets:
        url = t.get("url", "")
        if t.get("type") == "page" and ("tauri" in url or "index" in url):
            return t["webSocketDebuggerUrl"]
    for t in targets:
        if t.get("type") == "page":
            return t["webSocketDebuggerUrl"]
    raise SystemExit("no page target")


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


def main():
    ws = websocket.create_connection(find_page(), timeout=30, suppress_origin=True)
    try:
        r = invoke(ws, "loomy_wechat_login")
        print("loomy_wechat_login ->", json.dumps(r, ensure_ascii=False))
        url = (r or {}).get("authUrl")
        if not url:
            print("FAIL: no authUrl returned")
            return
        print("fetching", url)
        html = urllib.request.urlopen(url, timeout=90).read().decode("utf-8", "replace")
        m = re.search(r'data:(image/\w+);base64,([A-Za-z0-9+/=]+)', html)
        if m:
            print("OK  QR image mime=%s base64_len=%d" % (m.group(1), len(m.group(2))))
        else:
            print("FAIL: page has no embedded QR image. First 1200 chars:")
            print(html[:1200])
        # Poll once to confirm the status ladder is reachable.
        poll = urllib.request.urlopen(url.replace("/wechat/qr", "/wechat/poll"), timeout=60)
        print("poll ->", poll.read().decode("utf-8", "replace")[:300])
    finally:
        ws.close()


if __name__ == "__main__":
    main()
