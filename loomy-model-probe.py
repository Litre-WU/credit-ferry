import json, urllib.request, urllib.error, uuid, time

creds = json.load(open(r'C:/Users/Administrator/AppData/Roaming/CreditFerry/credentials.json', encoding='utf-8'))
SESSION = creds['loomy']['sessions'][0]['session']
models = json.load(open(r'C:/Users/Administrator/AppData/Roaming/CreditFerry/models-cache.json', encoding='utf-8'))['loomy']
BASE = 'https://loomyad.xunfei.cn/api/v1'

def probe(mid):
    headers = {
        'Authorization': f'Bearer {SESSION}',
        'token': SESSION,
        'traceparent': f'00-{uuid.uuid4().hex}-{uuid.uuid4().hex[:16]}-01',
        'loomy-version': '0.9.38',
        'Content-Type': 'application/json',
    }
    body = {'model': mid, 'messages': [{'role': 'user', 'content': 'hi'}], 'max_tokens': 8}
    req = urllib.request.Request(BASE + '/chat/completions', data=json.dumps(body).encode(), headers=headers, method='POST')
    try:
        with urllib.request.urlopen(req, timeout=60) as r:
            return r.status, ''
    except urllib.error.HTTPError as e:
        return e.code, e.read().decode('utf-8', 'replace')[:120]
    except Exception as e:
        return 0, str(e)[:120]

for m in models:
    mid = m['upstream_id']
    status, detail = probe(mid)
    mark = 'OK ' if status == 200 else 'FAIL'
    print(f'{mark} | {mid:32s} | {m["name"][:30]:30s} | {status} {detail if status != 200 else ""}')
    time.sleep(0.3)
