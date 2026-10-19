"""全渠道冒烟测试：每个已登录渠道抽 1 个发布中的模型，非流式 + 流式各打一发。"""
import json
import urllib.request

BASE = "http://127.0.0.1:6666/v1"
KEY = open(r"C:\Users\Administrator\AppData\Roaming\CreditFerry\api-key", encoding="utf-8").read().strip()
H = {"Authorization": "Bearer " + KEY, "Content-Type": "application/json"}


def models():
    d = json.load(urllib.request.urlopen(urllib.request.Request(BASE + "/models", headers=H), timeout=30))
    return d["data"]


def chat(mid, stream=False):
    payload = {
        "model": mid,
        "messages": [{"role": "user", "content": "用一句话回答：1+1等于几？"}],
        "stream": stream,
        "max_tokens": 128,
    }
    req = urllib.request.Request(BASE + "/chat/completions", data=json.dumps(payload).encode(), headers=H)
    r = urllib.request.urlopen(req, timeout=150)
    if not stream:
        body = json.loads(r.read().decode("utf-8", "replace"))
        ch = body.get("choices", [{}])[0]
        msg = ch.get("message", {})
        out = (msg.get("content") or msg.get("reasoning_content") or "")
        return r.status, out[:40], ch.get("finish_reason")
    parts = []
    finish = None
    for raw in r:
        line = raw.decode("utf-8", "replace").strip()
        if not line.startswith("data:"):
            continue
        p = line[5:].strip()
        if p == "[DONE]":
            break
        try:
            o = json.loads(p)
        except json.JSONDecodeError:
            continue
        ch = o.get("choices", [])
        if ch:
            dt = ch[0].get("delta", {})
            if dt.get("content"):
                parts.append(dt["content"])
            elif dt.get("reasoning_content"):
                parts.append(dt["reasoning_content"])
            if ch[0].get("finish_reason"):
                finish = ch[0]["finish_reason"]
    return r.status, "".join(parts)[:40], finish


def main():
    ms = models()
    print(f"published total: {len(ms)}")
    by_src = {}
    for m in ms:
        by_src.setdefault(m.get("owned_by", "?"), []).append(m["id"])
    targets = []
    order = ["loomy", "raccoon", "trae", "lobsterai", "minimax", "buddy", "workbuddy",
             "maodao", "opencode", "kilo", "zen", "cline", "freebuff"]
    for s in order:
        if by_src.get(s):
            targets.append((s, by_src[s][0]))
    results = {}
    for src, mid in targets:
        ns = st = None
        try:
            ns = chat(mid, False)
        except Exception as e:
            ns = ("ERR", repr(e)[:60], None)
        try:
            st = chat(mid, True)
        except Exception as e:
            st = ("ERR", repr(e)[:60], None)
        ok_n = ns[0] == 200 and ns[1]
        ok_s = st[0] == 200 and st[1]
        mark = "PASS" if (ok_n and ok_s) else ("PART" if (ok_n or ok_s) else "FAIL")
        results[src] = mark
        print(f"[{mark}] {src:10s} {mid[:34]:34s} 非流 {ns[0]}/{str(ns[1])[:18]!r}  流 {st[0]}/{str(st[1])[:18]!r}")
    passed = sum(1 for v in results.values() if v == "PASS")
    print(f"\n{passed}/{len(targets)} PASS")
    return 0 if passed >= len(targets) - 1 else 1


if __name__ == "__main__":
    raise SystemExit(main())
