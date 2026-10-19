#!/usr/bin/env python3
"""多把 API Key 端到端验证：
1. CDP invoke add_api_key 新建一把「E2E测试」
2. 用新密钥直连转发口 /v1/models → 期望 200
3. snapshot.apiKeys 应含 2 条（主+额外），额外密钥在列表里是掩码
4. reveal_api_key 取回全文 → 与新建值一致
5. revoke_api_key 吊销 → 再用该密钥请求 → 期望 401
6. 主密钥始终 200
"""
import importlib.util
import json
import sys
import time
import urllib.request

import websocket

spec = importlib.util.spec_from_file_location("cdpdrive", "cdp-drive.py")
cd = importlib.util.module_from_spec(spec)
spec.loader.exec_module(cd)

BASE = "http://127.0.0.1:6666/v1"


def probe(key):
    req = urllib.request.Request(BASE + "/models", headers={"Authorization": "Bearer " + key})
    try:
        r = urllib.request.urlopen(req, timeout=15)
        return r.status
    except urllib.error.HTTPError as e:
        return e.code


def main():
    ws = websocket.create_connection(cd.find_page(), timeout=30, suppress_origin=True)
    fails = []

    main_key = open(r"C:\Users\Administrator\AppData\Roaming\CreditFerry\api-key", encoding="utf-8").read().strip()

    # 1) 新增
    r = cd.invoke(ws, "add_api_key", {"label": "E2E测试"})
    if isinstance(r, dict) and r.get("__err"):
        print("FAIL add:", r); return 1
    new_key = r["key"]
    new_id = r["id"]
    print("1) 新增密钥 OK:", new_key[:8] + "…", "id:", new_id[:8])
    print("   banner DOM:", cd.evaluate(ws, """(function(){var b=document.getElementById('newkey-banner');
        return JSON.stringify({shown: b.style.display, hasKey: b.innerHTML.indexOf('code')>=0});})()""", await_promise=False))

    # 2) 新密钥立即可用（无需重启）
    st = probe(new_key)
    print("2) 新密钥请求 /v1/models:", st, "→", "PASS" if st == 200 else "FAIL")
    if st != 200:
        fails.append("新密钥未被放行")

    # 3) snapshot 掩码
    snap = cd.invoke(ws, "get_snapshot")
    keys = snap.get("apiKeys", [])
    extras = [k for k in keys if not k["primary"]]
    prim = [k for k in keys if k["primary"]]
    masked = extras[0]["key"] if extras else None
    print("3) apiKeys 条数:", len(keys), "主:", len(prim), "额外:", len(extras),
          "额外显示:", masked, "revealed:", extras[0]["revealed"] if extras else None)
    if len(keys) < 2 or masked == new_key:
        fails.append("列表未掩码或缺条目")

    # 4) reveal 取回
    rv = cd.invoke(ws, "reveal_api_key", {"id": new_id})
    ok = rv.get("key") == new_key
    print("4) reveal 一致:", "PASS" if ok else "FAIL", str(rv)[:60])
    if not ok:
        fails.append("reveal 不符")

    # 5) 吊销 → 401
    rr = cd.invoke(ws, "revoke_api_key", {"id": new_id})
    st2 = probe(new_key)
    print("5) 吊销:", rr, "吊销后请求:", st2, "→", "PASS" if st2 == 401 else "FAIL")
    if st2 != 401:
        fails.append("吊销后仍放行")

    # 6) 主密钥不受影响
    st3 = probe(main_key)
    print("6) 主密钥:", st3, "→", "PASS" if st3 == 200 else "FAIL")
    if st3 != 200:
        fails.append("主密钥失效")

    # 7) 吊销主密钥应被拒
    snap = cd.invoke(ws, "get_snapshot")
    pid = [k for k in snap["apiKeys"] if k["primary"]][0]["id"]
    rej = cd.invoke(ws, "revoke_api_key", {"id": pid})
    print("7) 吊销主密钥被拒:", rej)
    if not (isinstance(rej, dict) and rej.get("__err")):
        fails.append("主密钥可被吊销!")

    # 列表 DOM
    print("8) 表格行:", cd.evaluate(ws, "document.querySelectorAll('#keys-table tbody tr').length", await_promise=False))

    print("\n=== " + ("ALL PASS" if not fails else "FAILS: " + "; ".join(fails)))
    return 0 if not fails else 1


if __name__ == "__main__":
    sys.exit(main())
