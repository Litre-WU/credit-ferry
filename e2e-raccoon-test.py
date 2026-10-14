import json, urllib.request, urllib.error

KEY = open(r'C:/Users/Administrator/AppData/Roaming/CreditFerry/api-key', encoding='utf-8').read().strip()
BASE = 'http://127.0.0.1:41990'

def post(payload, timeout=180):
    req = urllib.request.Request(BASE + '/v1/chat/completions', data=json.dumps(payload).encode('utf-8'),
                                 headers={'Content-Type': 'application/json', 'Authorization': f'Bearer {KEY}'}, method='POST')
    try:
        with urllib.request.urlopen(req, timeout=timeout) as r:
            return json.loads(r.read().decode('utf-8'))
    except urllib.error.HTTPError as e:
        return {'__error': e.code, '__body': e.read().decode('utf-8', 'replace')[:400]}

# 小浣熊端到端：纯对话（消耗约 1-5 积分）
d = post({'model': 'Raccoon · sn-sensenova-6-8-flash-lite',
          'messages': [{'role': 'user', 'content': '用一句简体中文回答：1+1=?'}], 'max_tokens': 100})
if '__error' in d:
    print('HTTP', d['__error'], d['__body'])
else:
    ch = d.get('choices', [{}])[0]
    msg = ch.get('message', {})
    print('content:', (msg.get('content') or '')[:200])
    print('finish_reason:', ch.get('finish_reason'), '| usage:', d.get('usage'))
