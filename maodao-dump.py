#!/usr/bin/env python3
"""只读 dump 码道客户端 safeStorage 解密后的结构（敏感值打码）。"""
import base64
import ctypes
import ctypes.wintypes as wintypes
import json
import os


class DATA_BLOB(ctypes.Structure):
    _fields_ = [("cbData", wintypes.DWORD), ("pbData", ctypes.POINTER(ctypes.c_char))]


def dpapi_unprotect(raw: bytes) -> bytes:
    crypt = ctypes.windll.crypt32.CryptUnprotectData
    crypt.restype = wintypes.BOOL
    kernel = ctypes.windll.kernel32
    buf = ctypes.create_string_buffer(raw, len(raw))
    blob_in = DATA_BLOB(len(raw), ctypes.cast(buf, ctypes.POINTER(ctypes.c_char)))
    blob_out = DATA_BLOB()
    ok = crypt(ctypes.byref(blob_in), None, None, None, None, 0x01, ctypes.byref(blob_out))
    if not ok:
        raise RuntimeError("CryptUnprotectData failed")
    try:
        if not blob_out.pbData or blob_out.cbData == 0:
            return b""
        return ctypes.string_at(blob_out.pbData, int(blob_out.cbData))
    finally:
        if blob_out.pbData:
            kernel.LocalFree(blob_out.pbData)


def os_crypt_key() -> bytes:
    path = os.path.join(os.environ["APPDATA"], "codearts", "Local State")
    doc = json.load(open(path, encoding="utf-8"))
    blob = base64.b64decode(doc["os_crypt"]["encrypted_key"])
    if blob.startswith(b"DPAPI"):
        blob = blob[5:]
    return dpapi_unprotect(blob)


def decrypt(key: bytes, blob: bytes) -> bytes:
    from cryptography.hazmat.primitives.ciphers.aead import AESGCM

    return AESGCM(key).decrypt(blob[3:15], blob[15:], b"")


def main() -> None:
    key = os_crypt_key()
    sd = os.path.join(os.environ["APPDATA"], "codearts", "secretStorage")
    for name in sorted(os.listdir(sd)):
        if not name.endswith(".enc"):
            continue
        raw = open(os.path.join(sd, name), encoding="utf-8").read().strip()
        doc = json.loads(decrypt(key, base64.b64decode(raw)).decode())

        def walk(o, path=""):
            if isinstance(o, dict):
                for k, v in o.items():
                    yield from walk(v, path + "." + k)
            elif isinstance(o, list):
                for i, v in enumerate(o):
                    yield from walk(v, "%s[%d]" % (path, i))
            else:
                yield path, o

        print("blob:", name)
        for path, value in walk(doc):
            text = str(value)
            if len(text) > 48:
                text = text[:16] + "…(" + str(len(text)) + ")"
            print("  ", path, "=", text)


if __name__ == "__main__":
    main()
