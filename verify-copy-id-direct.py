#!/usr/bin/env python3
"""直连 /v1 验证（不依赖 CDP）：修复后复制文本=完整客户端 id。
① /v1/models 里发布的 id 全部含 " · " 前缀，逐个作为复制源直连 chat 路由成功（抽查）
② 反证：去掉前缀的纯显示名（旧复制行为）→ 400，复现用户报障
③ 转发口对面板新 exe 已生效（6666 存活）"""
import json, os, urllib.request

ok = True
def check(name, cond, detail=""):
    global ok
    print(("PASS" if cond else "FAIL"), name, str(detail)[:170])
    ok = ok and cond

BASE = "http://127.0.0.1:6666"
key = open(os.path.expandvars(r"%APPDATA%\CreditFerry\api-key"), encoding="utf-8").read().strip()

def api(path, payload=None, timeout=120):
    req = urllib.request.Request(BASE + path,
        data=json.dumps(payload).encode() if payload is not None else None,
        headers={"Authorization": "Bearer " + key, "Content-Type": "application/json"})
    try:
        with urllib.request.urlopen(req, timeout=timeout) as r:
            return r.status, json.loads(r.read())
    except urllib.error.HTTPError as e:
        return e.code, e.read().decode(errors="replace")[:200]

# ① 面板列表的 clientId 集 = /v1/models 发布集；复制按钮现在复制的就是它
st, models = api("/v1/models")
ids = [d["id"] for d in models.get("data", [])] if st == 200 else []
check("/v1/models 存活且已发布>0", st == 200 and len(ids) > 0, f"{st} n={len(ids)}")
check("发布 id 全含渠道前缀", all(" · " in i for i in ids), (ids[0] if ids else ""))

# ② 抽查 3 个发布 id 直连 chat：复制→粘贴→调用 全链路必须通
pick = ids[:: max(1, len(ids)//3)][:3]
for mid in pick:
    st2, body = api("/v1/chat/completions", {"model": mid, "messages": [{"role": "user", "content": "回复两个字"}], "max_tokens": 16})
    good = st2 == 200 and isinstance(body, dict) and "choices" in body
    check(f"复制id直连chat[{mid}]", good, body if not good else body["choices"][0]["message"]["content"][:20])

# ③ 反证（用户原报障路径）：纯显示名（无前缀）→ 400「Select an available model」
bare = pick[0].split(" · ", 1)[1] if pick else ""
st3, body3 = api("/v1/chat/completions", {"model": bare, "messages": [{"role": "user", "content": "hi"}], "max_tokens": 8})
check("反证:纯显示名被拒400", st3 == 400 and "Select" in str(body3), f"{st3} {body3}")

print("\n=== ALL PASS ===" if ok else "=== HAS FAIL ===")
raise SystemExit(0 if ok else 1)
