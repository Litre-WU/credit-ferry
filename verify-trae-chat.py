"""Task #50 验证脚本：TRAE chat 端到端（非流式 + 流式）。

用法：python verify-trae-chat.py [model_id]
默认 model_id = "TRAE · GLM-5.3-Flash"（probeOk 列表内的模型）。
"""
import json
import sys
import urllib.request

BASE = "http://127.0.0.1:6666/v1"
KEY_PATH = r"C:\Users\Administrator\AppData\Roaming\CreditFerry\api-key"

with open(KEY_PATH, "r", encoding="utf-8") as f:
    KEY = f.read().strip()


def post(payload: dict, stream: bool = False):
    req = urllib.request.Request(
        BASE + "/chat/completions",
        data=json.dumps(payload).encode("utf-8"),
        headers={
            "Authorization": "Bearer " + KEY,
            "Content-Type": "application/json",
        },
    )
    resp = urllib.request.urlopen(req, timeout=120)
    return resp


def main():
    model = sys.argv[1] if len(sys.argv) > 1 else "TRAE · GLM-5.3-Flash"

    # ---- 1. 非流式 ----
    payload = {
        "model": model,
        "messages": [{"role": "user", "content": "用一句话回答：1+1等于几？"}],
        "stream": False,
        "max_tokens": 64,
    }
    print(f"[1] 非流式  model={model}")
    try:
        r = post(payload)
        body = json.loads(r.read().decode("utf-8"))
        choice = body.get("choices", [{}])[0]
        msg = choice.get("message", {})
        content = msg.get("content")
        reasoning = msg.get("reasoning_content")
        usage = body.get("usage")
        print(f"    HTTP {r.status}")
        print(f"    finish_reason={choice.get('finish_reason')}")
        print(f"    content={str(content)[:120]!r}")
        if reasoning:
            print(f"    reasoning={str(reasoning)[:120]!r}")
        print(f"    usage={usage}")
        ok_nonstream = bool(content) and r.status == 200
    except urllib.error.HTTPError as e:
        print(f"    HTTP {e.code}: {e.read().decode('utf-8', 'replace')[:400]}")
        ok_nonstream = False

    # ---- 2. 流式 ----
    payload2 = dict(payload)
    payload2["stream"] = True
    print(f"\n[2] 流式    model={model}")
    ok_stream = False
    try:
        r = post(payload2, stream=True)
        chunks = 0
        text_parts = []
        got_done = False
        saw_usage = None
        for raw in r:
            line = raw.decode("utf-8", "replace").strip()
            if not line.startswith("data:"):
                continue
            data = line[5:].strip()
            if data == "[DONE]":
                got_done = True
                break
            try:
                obj = json.loads(data)
            except json.JSONDecodeError:
                continue
            chunks += 1
            ch = obj.get("choices", [{}])
            if ch:
                delta = ch[0].get("delta", {})
                if delta.get("content"):
                    text_parts.append(delta["content"])
            if obj.get("usage"):
                saw_usage = obj["usage"]
        text = "".join(text_parts)
        print(f"    chunks={chunks} done={got_done} usage={saw_usage}")
        print(f"    text={text[:120]!r}")
        ok_stream = chunks > 0 and got_done and bool(text)
    except urllib.error.HTTPError as e:
        print(f"    HTTP {e.code}: {e.read().decode('utf-8', 'replace')[:400]}")
    except Exception as e:  # noqa: BLE001
        print(f"    ERROR: {e!r}")

    print("\n=== 结果 ===")
    print(f"非流式: {'PASS' if ok_nonstream else 'FAIL'}")
    print(f"流式:   {'PASS' if ok_stream else 'FAIL'}")
    return 0 if (ok_nonstream and ok_stream) else 1


if __name__ == "__main__":
    sys.exit(main())
