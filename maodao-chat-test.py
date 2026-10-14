#!/usr/bin/env python3
"""用码道 gateway/config 返回的真实 model_id 逐个验证 chat 可调用性。"""
import importlib.util
import json
import sys

spec = importlib.util.spec_from_file_location("ml", "maodao-login.py")
ml = importlib.util.module_from_spec(spec)
spec.loader.exec_module(ml)

SNAP_CHAT = ml.SNAP_ENGINE + "/api/v2/chat/completions"
BENEFIT_CHAT = ml.BENEFIT_API + "/v2/chat/completions"
CONFIG = ml.BENEFIT_API + "/api/v1/gateway/config"


def main() -> int:
    session = json.load(open(ml.os.path.join(ml.os.environ["APPDATA"], "CreditFerry", "maodao-session.json"), encoding="utf-8"))
    creds = {
        "access_key_id": session["accessKey"],
        "secret_access_key": session["secretKey"],
        "security_token": session["securityToken"],
    }

    status, payload = ml.call("CONFIG", creds, "GET", CONFIG)
    document = json.loads(payload.decode("utf-8"))
    models = document.get("result", {}).get("models", [])
    base_url = document.get("result", {}).get("base_url", "")
    print("\nbase_url:", base_url, "models:", [m.get("model_id") for m in models])

    for model in models:
        model_id = model.get("model_id", "")
        for label, url in (("snap-access", SNAP_CHAT), ("opengw-v2", BENEFIT_CHAT)):
            body = json.dumps(
                {
                    "model": model_id,
                    "messages": [{"role": "user", "content": "say ok"}],
                    "max_tokens": 16,
                    "stream": False,
                }
            ).encode()
            status, payload = ml.call("CHAT %s %s" % (label, model_id), creds, "POST", url, body)
    return 0


if __name__ == "__main__":
    sys.exit(main())
