#!/usr/bin/env python3
"""码道 (CodeArts Doer) OAuth loopback 登录 + 全链路验证。

流程（与桌面客户端一致）：
  1. 本地生成 PKCE (SHA-256) 与 DPoP EC P-256 密钥对；
  2. 打开浏览器到 portal /authorize（携带 code_challenge 与回调端口）；
  3. 本地 127.0.0.1 回调服务器接住 ?code=...；
  4. 用 DPoP 证明到 STS /v1/oauth2/tokens 换临时 AK/SK/SecurityToken；
  5. 验证：刷新 → gateway/config 模型目录 → 余额 → 最小 chat。

成功后把会话写到 %APPDATA%\\CreditFerry\\maodao-session.json。
"""

import base64
import json
import os
import subprocess
import sys
import threading
import time
import urllib.parse
import urllib.request
import webbrowser
from http.server import BaseHTTPRequestHandler, HTTPServer

PORTAL_HOST = "https://codearts.huaweicloud.com/portal"
IAM_STS_HOST = "https://sts.cn-north-4.myhuaweicloud.com"
SNAP_ENGINE = "https://snap-access.cn-north-4.myhuaweicloud.com"
BENEFIT_API = "https://opengw.developer.huaweicloud.com"
CLIENT_ID = "codearts"
AUTH_REDIRECT_PATH = "/oauth/callback"
PLUGIN_NAME = "snap_AIIDE"
PLUGIN_VERSION = "5.1.0"
CALLBACK_PORT = 15321
LOGIN_TIMEOUT_MS = 300000

BASE_HEADERS = {
    "Content-Type": "application/json",
    "Accept": "application/json",
    "X-Language": "zh-cn",
    "ide-name": "codearts_space",
    "plugin-name": PLUGIN_NAME,
    "plugin-version": PLUGIN_VERSION,
    "maas_type": "benefit",
}

URI_SAFE = "!'()*-._~"


# ---------------------------------------------------------------- helpers
def b64url(data: bytes) -> str:
    return base64.urlsafe_b64encode(data).decode("ascii").rstrip("=")


def sha256_hex(data: bytes) -> str:
    import hashlib

    return hashlib.sha256(data).hexdigest()


def hmac_sha256(key: bytes, message: bytes) -> bytes:
    import hashlib as _h
    import hmac as _m

    return _m.new(key, message, _h.sha256).digest()


def sdk_now() -> str:
    return time.strftime("%Y%m%dT%H%M%SZ", time.gmtime())


def canonical_uri(path: str) -> str:
    encoded = "/".join(urllib.parse.quote(p, safe=URI_SAFE) for p in path.split("/"))
    return encoded if encoded.endswith("/") else encoded + "/"


def canonical_query(query: str) -> str:
    pairs = urllib.parse.parse_qsl(query, keep_blank_values=True)
    encoded = ["%s=%s" % (urllib.parse.quote(k, safe=URI_SAFE), urllib.parse.quote(v, safe=URI_SAFE)) for k, v in pairs]
    return "&".join(sorted(encoded))


def sign_request(method: str, url: str, headers: dict, body: bytes, ak: str, sk: str) -> dict:
    parsed = urllib.parse.urlsplit(url)
    time_str = sdk_now()
    signing = dict(headers)
    signing["host"] = parsed.netloc
    signing["X-Sdk-Date"] = time_str
    entries = sorted(((k.lower(), str(v).strip()) for k, v in signing.items()), key=lambda i: i[0])
    block = "".join("%s:%s\n" % (k, v) for k, v in entries)
    names = ";".join(k for k, _ in entries)
    canonical = "\n".join(
        [method.upper(), canonical_uri(parsed.path or "/"), canonical_query(parsed.query), block, names, sha256_hex(body)]
    )
    string_to_sign = "SDK-HMAC-SHA256\n%s\n%s" % (time_str, sha256_hex(canonical.encode()))
    signature = hmac_sha256(sk.encode(), string_to_sign.encode()).hex()
    out = dict(headers)
    out["X-Sdk-Date"] = time_str
    out["Authorization"] = "SDK-HMAC-SHA256 Access=%s, SignedHeaders=%s, Signature=%s" % (ak, names, signature)
    return out


def http(method: str, url: str, headers: dict, body: bytes = b"", timeout: float = 90.0):
    import ssl
    import urllib.error

    request = urllib.request.Request(url, data=body or None, method=method)
    for key, value in headers.items():
        request.add_header(key, value)
    context = ssl.create_default_context()
    context.check_hostname = False
    context.verify_mode = ssl.CERT_NONE
    try:
        with urllib.request.urlopen(request, timeout=timeout, context=context) as response:
            return response.status, response.read()
    except urllib.error.HTTPError as error:
        return error.code, error.read()


# ---------------------------------------------------------------- crypto
def generate_keypair():
    from cryptography.hazmat.primitives.asymmetric import ec

    key = ec.generate_private_key(ec.SECP256R1())
    numbers = key.private_numbers()
    public = numbers.public_numbers
    jwk = {
        "kty": "EC",
        "crv": "P-256",
        "x": b64url(public.x.to_bytes(32, "big")),
        "y": b64url(public.y.to_bytes(32, "big")),
        "d": b64url(numbers.private_value.to_bytes(32, "big")),
    }
    return key, jwk


def public_jwk(private_jwk: dict) -> dict:
    return {"kty": private_jwk["kty"], "crv": private_jwk["crv"], "x": private_jwk["x"], "y": private_jwk["y"]}


def load_private(jwk: dict):
    from cryptography.hazmat.primitives import serialization
    from cryptography.hazmat.primitives.asymmetric import ec

    def dec(value: str) -> int:
        raw = value + "=" * (-len(value) % 4)
        return int.from_bytes(base64.urlsafe_b64decode(raw), "big")

    numbers = ec.EllipticCurvePrivateNumbers(
        dec(jwk["d"]),
        ec.EllipticCurvePublicNumbers(dec(jwk["x"]), dec(jwk["y"]), ec.SECP256R1()),
    )
    der = numbers.private_key().private_bytes(
        encoding=serialization.Encoding.DER,
        format=serialization.PrivateFormat.PKCS8,
        encryption_algorithm=serialization.NoEncryption(),
    )
    return serialization.load_der_private_key(der, password=None)


def dpop_proof(private_key, jwk_pub: dict, htm: str, htu: str) -> str:
    from cryptography.hazmat.primitives import hashes
    from cryptography.hazmat.primitives.asymmetric import ec

    import secrets as _secrets

    header = {"alg": "ES256", "typ": "dpop+jwt", "jwk": jwk_pub}
    payload = {
        "htm": htm,
        "htu": htu,
        "iat": int(time.time()),
        "jti": _secrets.token_hex(16),
    }
    signing_input = ".".join(
        [
            b64url(json.dumps(header, separators=(",", ":")).encode()),
            b64url(json.dumps(payload, separators=(",", ":")).encode()),
        ]
    )
    der_sig = private_key.sign(signing_input.encode(), ec.ECDSA(hashes.SHA256()))

    if der_sig[0] != 0x30:
        raise RuntimeError("ECDSA 签名不是 DER 序列")
    pos = 1
    length = der_sig[pos]
    if length & 0x80:
        pos += 1 + (length & 0x7F)
    else:
        pos += 1

    def read_int(pos: int):
        if der_sig[pos] != 0x02:
            raise RuntimeError("DER 结构异常")
        length = der_sig[pos + 1]
        start = pos + 2
        return der_sig[start : start + length], start + length

    r_raw, pos = read_int(pos)
    s_raw, _ = read_int(pos)
    r = int.from_bytes(r_raw, "big")
    s = int.from_bytes(s_raw, "big")
    return signing_input + "." + b64url(r.to_bytes(32, "big") + s.to_bytes(32, "big"))


# ---------------------------------------------------------------- flow
class Callback:
    def __init__(self) -> None:
        self.event = threading.Event()
        self.result: dict | None = None


captured = Callback()


class Handler(BaseHTTPRequestHandler):
    def do_GET(self) -> None:  # noqa: N802
        parsed = urllib.parse.urlsplit(self.path)
        query = urllib.parse.parse_qs(parsed.query)
        if parsed.path == AUTH_REDIRECT_PATH:
            code = query.get("code", [None])[0]
            secret = query.get("secret", [None])[0]
            redirect = query.get("redirect", [None])[0]
            if code:
                captured.result = {"code": code}
                captured.event.set()
                self._send("登录成功，可以关闭这个页面了。")
            elif secret:
                captured.result = {"secret": secret, "redirect": redirect}
                captured.event.set()
                self._send("登录成功，可以关闭这个页面了。")
            else:
                self._send("回调缺少参数。", status=400)
        else:
            self._send("Not found", status=404)

    def _send(self, text: str, status: int = 200) -> None:
        body = text.encode("utf-8")
        self.send_response(status)
        self.send_header("Content-Type", "text/plain; charset=utf-8")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def log_message(self, *args) -> None:  # keep the console clean
        pass


def exchange_token(code: str, code_verifier: str, dpop_pair) -> dict:
    private_key, private_jwk = dpop_pair
    url = IAM_STS_HOST + "/v1/oauth2/tokens"
    proof = dpop_proof(private_key, public_jwk(private_jwk), "POST", url)
    form = urllib.parse.urlencode(
        {
            "client_id": CLIENT_ID,
            "code": code,
            "code_verifier": code_verifier,
            "grant_type": "authorization_code",
            "redirect_uri": "http://127.0.0.1:%d%s" % (CALLBACK_PORT, AUTH_REDIRECT_PATH),
        }
    ).encode()
    headers = {"DPoP": proof, "Content-Type": "application/x-www-form-urlencoded"}
    status, payload = http("POST", url, headers, form)
    print("EXCHANGE", status, payload[:800].decode("utf-8", "replace"))
    if status != 200:
        raise SystemExit(1)
    return json.loads(payload.decode("utf-8"))


def get_user_info(creds: dict) -> dict:
    ak = creds["access_key_id"]
    sk = creds["secret_access_key"]
    token = creds["security_token"]
    url = IAM_STS_HOST + "/v5/caller-identity"
    signed = sign_request("GET", url, {"X-Security-Token": token, "Content-Type": "application/json"}, b"", ak, sk)
    status, payload = http("GET", url, signed)
    print("USERINFO", status, payload[:400].decode("utf-8", "replace"))
    if status != 200:
        return {}
    return json.loads(payload.decode("utf-8"))


def refresh(refresh_token: str, dpop_pair) -> dict:
    private_key, private_jwk = dpop_pair
    url = IAM_STS_HOST + "/v1/oauth2/tokens"
    proof = dpop_proof(private_key, public_jwk(private_jwk), "POST", url)
    form = urllib.parse.urlencode(
        {
            "client_id": CLIENT_ID,
            "grant_type": "refresh_token",
            "refresh_token": refresh_token,
        }
    ).encode()
    headers = {"DPoP": proof, "Content-Type": "application/x-www-form-urlencoded"}
    status, payload = http("POST", url, headers, form)
    print("REFRESH", status, payload[:600].decode("utf-8", "replace"))
    if status != 200:
        raise SystemExit(1)
    return json.loads(payload.decode("utf-8"))


def call(section: str, creds: dict, method: str, url: str, body: bytes = b""):
    ak = creds.get("access_key_id") or creds.get("accessKey")
    sk = creds.get("secret_access_key") or creds.get("secretKey")
    token = creds.get("security_token") or creds.get("securityToken")
    headers = dict(BASE_HEADERS)
    headers["X-Security-Token"] = token
    headers["X-Auth-Token"] = token
    signed = sign_request(method, url, headers, body, ak, sk)
    status, payload = http(method, url, signed, body)
    print("\n%s %s -> %s" % (section, url, status))
    print(payload[:1500].decode("utf-8", "replace"))
    return status, payload


def main() -> int:
    import secrets

    code_verifier = secrets.token_hex(64)
    challenge = b64url(__import__("hashlib").sha256(code_verifier.encode()).digest())
    dpop_pair = generate_keypair()
    ticket_id = secrets.token_hex(32)

    login_url = (
        "%s/authorize?theme=2&locale=zh-cn&uri_scheme=%s&client_id=%s"
        "&port=%d&code_challenge=%s&code_challenge_method=SHA-256"
        "&ticket_id=%s&plugin-name=%s&plugin-version=%s"
        % (PORTAL_HOST, CLIENT_ID, CLIENT_ID, CALLBACK_PORT, challenge, ticket_id, PLUGIN_NAME, PLUGIN_VERSION)
    )
    print("登录地址：\n%s\n" % login_url)

    server = HTTPServer(("127.0.0.1", CALLBACK_PORT), Handler)
    thread = threading.Thread(target=server.serve_forever, daemon=True)
    thread.start()

    try:
        webbrowser.open(login_url)
    except Exception:  # noqa: BLE001
        pass

    print("等待浏览器回调（最多 5 分钟）...")
    if not captured.event.wait(LOGIN_TIMEOUT_MS / 1000):
        print("登录超时")
        return 1
    server.shutdown()

    result = captured.result or {}
    if "code" in result:
        token_response = exchange_token(result["code"], code_verifier, dpop_pair)
        creds = token_response["credentials"]
        refresh_token = token_response.get("refresh_token", "")
    else:
        raise SystemExit("ticket 方式暂未实现")

    import uuid as _uuid

    info = get_user_info(creds)
    session = {
        "id": str(_uuid.uuid4()),
        "userId": info.get("user_id") or info.get("principal_id") or "",
        "userName": info.get("user_name") or "",
        "domainId": info.get("account_id") or "",
        "accessKey": creds["access_key_id"],
        "secretKey": creds["secret_access_key"],
        "securityToken": creds["security_token"],
        "refreshToken": refresh_token,
        "expiresAt": int(time.time() * 1000) + 3600 * 1000,
        "clientId": CLIENT_ID,
        "stationType": info.get("station_type") or "HC",
        "createdAt": time.strftime("%Y-%m-%dT%H:%M:%S.000Z", time.gmtime()),
        "updatedAt": time.strftime("%Y-%m-%dT%H:%M:%S.000Z", time.gmtime()),
        "dpopPrivateKey": json.dumps(dpop_pair[1]),
        "dpopPublicKey": json.dumps(public_jwk(dpop_pair[1])),
    }

    out_dir = os.path.join(os.environ["APPDATA"], "CreditFerry")
    os.makedirs(out_dir, exist_ok=True)
    out_path = os.path.join(out_dir, "maodao-session.json")
    with open(out_path, "w", encoding="utf-8") as handle:
        json.dump(session, handle, ensure_ascii=False, indent=2)
    print("\n[+] 会话已写入:", out_path)
    print("   userName:", session["userName"], "userId:", session["userId"])

    # 刷新链路验证
    rotated = refresh(refresh_token, dpop_pair)
    print("\n[+] 刷新链路 OK，新 AK:", rotated["credentials"]["access_key_id"][:10] + "…")

    creds2 = rotated["credentials"]
    call("GATEWAY/CONFIG", creds2, "GET", BENEFIT_API + "/api/v1/gateway/config")
    call("BALANCE", creds2, "GET", BENEFIT_API + "/api/v1/user/tokens/balance")

    body = json.dumps(
        {"model": "DeepSeek-V3.2", "messages": [{"role": "user", "content": "hi"}], "max_tokens": 8}
    ).encode()
    call("CHAT", creds2, "POST", SNAP_ENGINE + "/api/v2/chat/completions", body)
    return 0


if __name__ == "__main__":
    sys.exit(main())
