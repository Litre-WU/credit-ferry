#!/usr/bin/env python3
"""触发「统一验证可调用性」并轮询进度，直到 verifyAllRunning=false。"""
import json
import importlib.util
import sys
import time

import websocket

spec = importlib.util.spec_from_file_location("cdpdrive", "cdp-drive.py")
cdpdrive = importlib.util.module_from_spec(spec)
spec.loader.exec_module(cdpdrive)


def main():
    max_wait = int(sys.argv[1]) if len(sys.argv) > 1 else 900
    ws = websocket.create_connection(cdpdrive.find_page(), timeout=30, suppress_origin=True)

    r = cdpdrive.invoke(ws, "verify_all_models")
    print("verify_all_models ->", json.dumps(r, ensure_ascii=False))

    start = time.time()
    while time.time() - start < max_wait:
        time.sleep(10)
        try:
            snap = cdpdrive.invoke(ws, "get_snapshot")
        except Exception as e:
            print("snapshot failed:", e)
            continue
        running = snap.get("verifyAllRunning")
        rows = []
        total_ok = 0
        for key, label in [("loomy","Loomy"),("raccoon","Raccoon"),("opencode","OC"),
                           ("maodao","Maodao"),("kilo","Kilo"),("zen","Zen"),
                           ("buddy","CodeBuddy"),("workbuddy","WorkBuddy"),
                           ("lobsterai","LobsterAI"),("trae","TRAE"),
                           ("minimax","MiniMax"),("cline","Cline")]:
            models = (snap.get(key) or {}).get("models", [])
            ok = sum(1 for m in models if m.get("probeOk") is True)
            total_ok += ok
            rows.append(f"{label}:{ok}/{len(models)}")
        print(f"[{int(time.time()-start):4d}s] running={running} 可调用合计={total_ok} | " + " ".join(rows))
        if not running:
            print("DONE")
            break
    return 0


if __name__ == "__main__":
    sys.exit(main())
