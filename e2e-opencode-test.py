import json, urllib.request

KEY = open(r'C:/Users/Administrator/AppData/Roaming/CreditFerry/api-key', encoding='utf-8').read().strip()
BASE = 'http://127.0.0.1:41990'

def post(payload, timeout=300):
    req = urllib.request.Request(BASE + '/v1/chat/completions', data=json.dumps(payload).encode('utf-8'),
                                 headers={'Content-Type': 'application/json', 'Authorization': f'Bearer {KEY}'}, method='POST')
    with urllib.request.urlopen(req, timeout=timeout) as r:
        return json.loads(r.read().decode('utf-8'))

def show(tag, d):
    ch = d.get('choices', [{}])[0]
    msg = ch.get('message', {})
    print(f'--- {tag} ---')
    print('content:', (msg.get('content') or '')[:200])
    for tc in msg.get('tool_calls') or []:
        print('tool_call:', tc['function']['name'], '|', tc['function']['arguments'][:120])
    print('finish_reason:', ch.get('finish_reason'), '| usage:', d.get('usage'))
    print()

# 1) OpenCode 免费模型：纯对话
d = post({'model': 'OC · Nemotron 3.5 Lightning Free',
          'messages': [{'role': 'user', 'content': '用一句简体中文回答：1+1=?'}], 'max_tokens': 200})
show('OpenCode 纯对话 (Nemotron 3.5 Lightning Free)', d)

# 2) OpenCode 免费模型：工具调用（信封协议）
tools = [{'type': 'function', 'function': {'name': 'get_weather', 'description': '查询城市天气',
          'parameters': {'type': 'object', 'properties': {'city': {'type': 'string', 'description': '城市名'}}, 'required': ['city']}}}]
d = post({'model': 'OC · Step 5 Preview Free',
          'messages': [{'role': 'user', 'content': '北京现在天气如何？请调用工具查询。'}],
          'tools': tools, 'tool_choice': 'auto', 'max_tokens': 400})
show('OpenCode 工具调用 (Step 5 Preview Free)', d)

# 3) 流式请求（OpenCode 路径的 SSE 包装）
req = urllib.request.Request(BASE + '/v1/chat/completions', data=json.dumps({
    'model': 'OC · Nemotron 3.5 Lightning Free',
    'messages': [{'role': 'user', 'content': '回复OK两个字'}], 'stream': True, 'max_tokens': 100}).encode('utf-8'),
    headers={'Content-Type': 'application/json', 'Authorization': f'Bearer {KEY}'}, method='POST')
chunks = 0
with urllib.request.urlopen(req, timeout=300) as r:
    print('--- OpenCode 流式 ---')
    print('content-type:', r.headers.get('content-type'))
    for line in r:
        line = line.decode('utf-8').strip()
        if line.startswith('data:') and line != 'data: [DONE]':
            chunks += 1
            if chunks <= 2:
                print(' ', line[:150])
print('SSE 分片数:', chunks)
