#!/usr/bin/env python3
"""测试码道内置模型目录（agent-center）是否可用。"""
import importlib.util
import json
import sys

spec = importlib.util.spec_from_file_location("ml", "maodao-login.py")
ml = importlib.util.module_from_spec(spec)
sys.modules["ml"] = ml
spec.loader.exec_module(ml)


def get(creds, url, extra=None):
    headers = dict(ml.BASE_HEADERS)
    headers["X-Security-Token"] = creds["security_token"]
    if extra:
        headers.update(extra)
    signed = ml.sign_request("GET", url, headers, b"", creds["access_key_id"], creds["secret_access_key"])
    return ml.http("GET", url, signed)


def main() -> int:
    session = json.load(
        open(ml.os.path.join(ml.os.environ["APPDATA"], "CreditFerry", "maodao-session.json"), encoding="utf-8")
    )
    creds = {
        "access_key_id": session["accessKey"],
        "secret_access_key": session["secretKey"],
        "security_token": session["securityToken"],
    }
    extra = {"Agent-Type": "AgentCenter"}

    for base in (ml.SNAP_ENGINE, ml.SNAP_ENGINE + "/snap-manager"):
        url = base + "/v1/agent-center/agents/useragents?offset=0&limit=100&is_primary_agent=true"
        status, payload = get(creds, url, extra)
        print("\n== %s -> %s" % (url, status))
        print(payload[:1200].decode("utf-8", "replace"))
        if status != 200:
            continue
        document = json.loads(payload.decode("utf-8"))
        agents = document.get("data", {}).get("agents") or document.get("agents") or []
        for agent in agents:
            name = agent.get("agent_name")
            alias = (agent.get("alias") or {}).get("alias_zh_cn")
            print("   agent:", name, alias, agent.get("agent_id"), agent.get("show_in_ide"))
    return 0


if __name__ == "__main__":
    sys.exit(main())
