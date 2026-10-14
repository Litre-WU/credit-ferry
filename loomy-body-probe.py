import json, urllib.request, urllib.error, uuid, sys

creds = json.load(open(r'C:/Users/Administrator/AppData/Roaming/CreditFerry/credentials.json', encoding='utf-8'))
SESSION = creds['loomy']['sessions'][0]['session']
BASE = 'https://loomyad.xunfei.cn/api/v1'
MODEL = 'DeepSeek-V4.1-Flash'

def call(tag, body):
    headers = {
        'Authorization': f'Bearer {SESSION}',
        'token': SESSION,
        'traceparent': f'00-{uuid.uuid4().hex}-{uuid.uuid4().hex[:16]}-01',
        'loomy-version': '0.9.38',
        'Content-Type': 'application/json',
    }
    req = urllib.request.Request(BASE + '/chat/completions', data=json.dumps(body).encode(), headers=headers, method='POST')
    try:
        with urllib.request.urlopen(req, timeout=60) as r:
            print(f'{tag:38s} -> {r.status} OK')
    except urllib.error.HTTPError as e:
        detail = e.read().decode('utf-8', 'replace')[:160]
        print(f'{tag:38s} -> {e.code} {detail}')
    except Exception as e:
        print(f'{tag:38s} -> ERR {e}')

base = {'model': MODEL, 'messages': [{'role': 'user', 'content': 'hi'}], 'max_tokens': 20}

call('A 最小 body', base)
call('B +temperature/top_p/n', {**base, 'temperature': 0.7, 'top_p': 0.9, 'n': 1})
call('C +stop/presence/frequency', {**base, 'stop': ['\n'], 'presence_penalty': 0, 'frequency_penalty': 0})
call('D +tools+tool_choice', {**base, 'tools': [{'type': 'function', 'function': {'name': 'get_weather', 'parameters': {'type': 'object', 'properties': {'city': {'type': 'string'}}}}}], 'tool_choice': 'auto'})
call('E +reasoning_effort', {**base, 'reasoning_effort': 'medium'})
call('F +stream_options', {**base, 'stream': False, 'stream_options': {'include_usage': True}})
call('G max_tokens=null', {**base, 'max_tokens': None})
call('H 无 max_tokens', {'model': MODEL, 'messages': [{'role': 'user', 'content': 'hi'}]})
call('I +user/logit_bias/seed', {**base, 'user': 'test', 'seed': 1})
call('J +enable_thinking', {**base, 'enable_thinking': True, 'chat_template_kwargs': {'enable_thinking': True}})
