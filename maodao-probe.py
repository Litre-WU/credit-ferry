#!/usr/bin/env python3
"""码道 (CodeArts Doer / snap-access) 协议探测脚本。

从本机 CodeArts 桌面客户端的 Electron safeStorage (Chromium OSCrypt v10)
里只读地恢复会话，然后：
  1. 用 DPoP 证明刷新 STS 临时凭证；
  2. 拉 gateway/config 模型目录；
  3. 拉额度余额；
  4. 发一条最小 chat 验证可调用。

本脚本只做读取与验证，不写任何凭据到客户端目录。
"""

import base64
import ctypes
import ctypes.wintypes as wintypes
import json
import os
import ssl
import sys
import time
import urllib.error
import urllib.parse
import urllib.request

APPDATA = os.environ.get("APPDATA", "")
CLIENT_ROOT = os.path.join(APPDATA, "codearts")
SNAP_ENGINE = "https://snap-access.cn-north-4.myhuaweicloud.com"
BENEFIT_API = "https://opengw.developer.huaweicloud.com"
STS_HOST = "https://sts.cn-north-4.myhuaweicloud.com"
TOKEN_PATH = "/v1/oauth2/tokens"

PLUGIN_NAME = "snap_AIIDE"
PLUGIN_VERSION = "5.1.0"
IDE_NAME = "codearts_space"


# ---------------------------------------------------------------- DPAPI
class DATA_BLOB(ctypes.Structure):
    _fields_ = [
        ("cbData", wintypes.DWORD),
        ("pbData", ctypes.POINTER(ctypes.c_char)),
    ]


def dpapi_unprotect(raw: bytes) -> bytes:
    crypt = ctypes.windll.crypt32.CryptUnprotectData
    crypt.restype = wintypes.BOOL
    kernel = ctypes.windll.kernel32

    in_buf = ctypes.create_string_buffer(raw, len(raw))
    blob_in = DATA_BLOB(len(raw), ctypes.cast(in_buf, ctypes.POINTER(ctypes.c_char)))
    blob_out = DATA_BLOB()

    ok = crypt(
        ctypes.byref(blob_in),
        None,
        None,
        None,
        None,
        0x01,  # CRYPTPROTECT_UI_FORBIDDEN
        ctypes.byref(blob_out),
    )
    if not ok:
        raise RuntimeError("CryptUnprotectData failed: %s" % ctypes.get_last_error())

    try:
        size = int(blob_out.cbData)
        if size == 0 or not blob_out.pbData:
            return b""
        return ctypes.string_at(blob_out.pbData, size)
    finally:
        if blob_out.pbData:
            kernel.LocalFree(blob_out.pbData)


def os_crypt_key() -> bytes:
    path = os.path.join(CLIENT_ROOT, "Local State")
    with open(path, "r", encoding="utf-8") as handle:
        document = json.load(handle)
    encoded = document.get("os_crypt", {}).get("encrypted_key")
    if not encoded:
        raise RuntimeError("Local State 缺少 os_crypt.encrypted_key")
    blob = base64.b64decode(encoded)
    if blob.startswith(b"DPAPI"):
        blob = blob[5:]
    return dpapi_unprotect(blob)


def os_crypt_decrypt(key: bytes, blob: bytes) -> bytes:
    from cryptography.hazmat.primitives.ciphers.aead import AESGCM

    if blob[:3] != b"v10":
        raise RuntimeError("不是 OSCrypt v10 格式")
    nonce = blob[3:15]
    return AESGCM(key).decrypt(nonce, blob[15:], b"")


def load_session() -> dict:
    key = os_crypt_key()
    secret_dir = os.path.join(CLIENT_ROOT, "secretStorage")
    for name in sorted(os.listdir(secret_dir)):
        if not name.endswith(".enc"):
            continue
        with open(os.path.join(secret_dir, name), "r", encoding="utf-8") as handle:
            raw = handle.read().strip()
        plain = os_crypt_decrypt(key, base64.b64decode(raw))
        document = json.loads(plain.decode("utf-8"))
        session = document.get("session")
        if isinstance(session, dict) and session.get("refreshToken"):
            session["_file"] = name
            # The DPoP key pair the client minted at login lives beside the
            # session; the refresh token is bound to its JWK thumbprint.
            context = document.get("loginContext") or {}
            pair = context.get("dpopKeyPair") or {}
            session["dpopPrivateKey"] = pair.get("privateKey", "")
            session["dpopPublicKey"] = pair.get("publicKey", "")
            return session
    raise RuntimeError("未找到含 refreshToken 的会话")


# ---------------------------------------------------------------- DPoP
def b64url(data: bytes) -> str:
    return base64.urlsafe_b64encode(data).decode("ascii").rstrip("=")


def b64url_hash(public_jwk: dict) -> str:
    """RFC 7638 JWK SHA-256 thumbprint — the value an STS refresh token binds to."""
    import hashlib

    canonical = json.dumps(
        {"crv": public_jwk["crv"], "kty": public_jwk["kty"], "x": public_jwk["x"], "y": public_jwk["y"]},
        separators=(",", ":"),
        sort_keys=True,
    )
    return b64url(hashlib.sha256(canonical.encode()).digest())


def _int_to_b32(value: int) -> bytes:
    return value.to_bytes(32, "big")


def load_dpop_key(private_jwk: str):
    from cryptography.hazmat.primitives.asymmetric import ec
    from cryptography.hazmat.primitives import serialization

    jwk = json.loads(private_jwk)
    if jwk.get("kty") != "EC" or jwk.get("crv") != "P-256":
        raise RuntimeError("DPoP 私钥不是 EC P-256")
    d = int.from_bytes(base64.urlsafe_b64decode(jwk["d"] + "=="), "big")
    x = int.from_bytes(base64.urlsafe_b64decode(jwk["x"] + "=="), "big")
    y = int.from_bytes(base64.urlsafe_b64decode(jwk["y"] + "=="), "big")

    pub_numbers = ec.EllipticCurvePublicNumbers(x, y, ec.SECP256R1())
    priv = ec.EllipticCurvePrivateNumbers(d, pub_numbers).private_key()

    # A DER/PKCS8 copy keeps the signing path simple.
    der = priv.private_bytes(
        encoding=serialization.Encoding.DER,
        format=serialization.PrivateFormat.PKCS8,
        encryption_algorithm=serialization.NoEncryption(),
    )
    loaded = serialization.load_der_private_key(der, password=None)
    return loaded, {"kty": "EC", "crv": "P-256", "x": b64url(_int_to_b32(x)), "y": b64url(_int_to_b32(y))}


def dpop_proof(private_key, public_jwk: dict, htm: str, htu: str) -> str:
    from cryptography.hazmat.primitives import hashes
    from cryptography.hazmat.primitives.asymmetric import ec

    header = {"typ": "dpop+jwt", "alg": "ES256", "jwk": public_jwk}
    payload = {
        "jti": b64url(bytes(range(16))) + str(int(time.time() * 1000)),
        "htm": htm,
        "htu": htu,
        "iat": int(time.time()),
    }
    signing_input = ".".join(
        [
            b64url(json.dumps(header, separators=(",", ":")).encode()),
            b64url(json.dumps(payload, separators=(",", ":")).encode()),
        ]
    )
    der_sig = private_key.sign(signing_input.encode(), ec.ECDSA(hashes.SHA256()))

    # DER SEQUENCE{r,s} → raw r||s (32+32).
    if der_sig[0] != 0x30:
        raise RuntimeError("ECDSA 签名不是 DER 序列")
    pos = 2  # skip tag + length (length is short-form for P-256)
    def read_int(pos: int):
        if der_sig[pos] != 0x02:
            raise RuntimeError("DER 结构异常")
        length = der_sig[pos + 1]
        start = pos + 2
        raw = der_sig[start : start + length]
        return raw, start + length
    r_raw, pos = read_int(pos)
    s_raw, _ = read_int(pos)
    r = int.from_bytes(r_raw, "big")
    s = int.from_bytes(s_raw, "big")
    raw_sig = _int_to_b32(r) + _int_to_b32(s)

    return signing_input + "." + b64url(raw_sig)


# ---------------------------------------------------------------- AKSK
def hmac_sha256(key: bytes, message: bytes) -> bytes:
    import hashlib
    import hmac as _hmac

    return _hmac.new(key, message, hashlib.sha256).digest()


def sha256_hex(data: bytes) -> str:
    import hashlib

    return hashlib.sha256(data).hexdigest()


def sdk_now() -> str:
    return time.strftime("%Y%m%dT%H%M%SZ", time.gmtime())


URI_SAFE = "!'()*-._~"


def canonical_uri(path: str) -> str:
    # Each segment is encodeURIComponent'd, then re-joined; a trailing "/" is
    # always present (this is APIG's canonical form, not RFC 3986).
    encoded = "/".join(urllib.parse.quote(part, safe=URI_SAFE) for part in path.split("/"))
    return encoded if encoded.endswith("/") else encoded + "/"


def canonical_query(query: str) -> str:
    pairs = urllib.parse.parse_qsl(query, keep_blank_values=True)
    encoded = ["%s=%s" % (urllib.parse.quote(k, safe=URI_SAFE), urllib.parse.quote(v, safe=URI_SAFE)) for k, v in pairs]
    return "&".join(sorted(encoded))


def canonical_headers(headers: dict):
    entries = sorted(
        ((k.lower(), str(v).strip()) for k, v in headers.items()),
        key=lambda item: item[0],
    )
    block = "".join("%s:%s\n" % (k, v) for k, v in entries)
    names = ";".join(k for k, _ in entries)
    return block, names


def sign_request(method: str, url: str, headers: dict, body: bytes, ak: str, sk: str) -> dict:
    parsed = urllib.parse.urlsplit(url)
    time_str = sdk_now()
    signing = dict(headers)
    signing["host"] = parsed.netloc
    signing["X-Sdk-Date"] = time_str
    block, names = canonical_headers(signing)
    body_hash = sha256_hex(body)
    canonical_request = "\n".join(
        [method.upper(), canonical_uri(parsed.path or "/"), canonical_query(parsed.query), block, names, body_hash]
    )
    digest = sha256_hex(canonical_request.encode())
    string_to_sign = "%s\n%s\n%s" % ("SDK-HMAC-SHA256", time_str, digest)
    signature = hmac_sha256(sk.encode(), string_to_sign.encode()).hex()
    out = dict(headers)
    out["X-Sdk-Date"] = time_str
    out["Authorization"] = "SDK-HMAC-SHA256 Access=%s, SignedHeaders=%s, Signature=%s" % (ak, names, signature)
    return out


def http(method: str, url: str, headers: dict, body: bytes = b"", timeout: float = 60.0):
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


# ---------------------------------------------------------------- main
BASE_HEADERS = {
    "Content-Type": "application/json",
    "Accept": "application/json",
    "X-Language": "zh-cn",
    "ide-name": IDE_NAME,
    "plugin-name": PLUGIN_NAME,
    "plugin-version": PLUGIN_VERSION,
    "maas_type": "benefit",
}


def main() -> int:
    session = load_session()
    print("session keys:", sorted(session.keys()))
    print("  userId  :", session.get("userId"))
    print("  userName:", session.get("userName"))
    print("  ak      :", str(session.get("accessKey"))[:10] + "…")
    print("  expires :", session.get("expiresAt"))

    refresh_token = session.get("refreshToken", "")

    dpop_private = session.get("dpopPrivateKey", "")
    if not dpop_private:
        print("[!] 会话里没有 DPoP 私钥，刷新令牌是 jkt 绑定的，无法续期")
        return 1

    private_key, public_jwk = load_dpop_key(dpop_private)
    print("  dpop jkt:", b64url_hash(public_jwk))
    htu = STS_HOST + TOKEN_PATH
    proof = dpop_proof(private_key, public_jwk, "POST", htu)

    form = urllib.parse.urlencode(
        {
            "client_id": "codearts",
            "grant_type": "refresh_token",
            "refresh_token": refresh_token,
        }
    ).encode()
    headers = {
        "Content-Type": "application/x-www-form-urlencoded",
        "Accept": "application/json",
        "DPoP": proof,
    }
    status, payload = http("POST", STS_HOST + TOKEN_PATH, headers, form)
    print("\nREFRESH", status, payload[:600].decode("utf-8", "replace"))
    if status != 200:
        return 1

    document = json.loads(payload.decode("utf-8"))
    creds = document.get("credentials", document)
    ak = creds.get("access_key_id") or creds.get("accessKeyId")
    sk = creds.get("secret_access_key") or creds.get("secretAccessKey")
    token = creds.get("security_token") or creds.get("securityToken")
    print("  new ak:", str(ak)[:10] + "…", "exp:", creds.get("expiration"))

    signed = sign_request(
        "GET",
        BENEFIT_API + "/api/v1/gateway/config",
        {"X-Security-Token": token, "X-Auth-Token": token, **BASE_HEADERS},
        b"",
        ak,
        sk,
    )
    status, payload = http("GET", BENEFIT_API + "/api/v1/gateway/config", signed)
    print("\nGATEWAY/CONFIG", status, payload[:800].decode("utf-8", "replace"))

    signed = sign_request(
        "GET",
        BENEFIT_API + "/api/v1/user/tokens/balance",
        {"X-Security-Token": token, "X-Auth-Token": token, **BASE_HEADERS},
        b"",
        ak,
        sk,
    )
    status, payload = http("GET", BENEFIT_API + "/api/v1/user/tokens/balance", signed)
    print("\nBALANCE", status, payload[:600].decode("utf-8", "replace"))

    body = json.dumps(
        {
            "model": "DeepSeek-V3.2",
            "messages": [{"role": "user", "content": "hi"}],
            "max_tokens": 8,
        }
    ).encode()
    signed = sign_request(
        "POST",
        SNAP_ENGINE + "/api/v2/chat/completions",
        {"X-Security-Token": token, "X-Auth-Token": token, **BASE_HEADERS},
        body,
        ak,
        sk,
    )
    status, payload = http("POST", SNAP_ENGINE + "/api/v2/chat/completions", signed, body)
    print("\nCHAT", status, payload[:800].decode("utf-8", "replace"))
    return 0


if __name__ == "__main__":
    sys.exit(main())
