#!/usr/bin/env python3
"""CDP 检查新前端：统一验证按钮、模型列表 DOM、旧板块移除、搜索/翻页交互。"""
import json
import subprocess
import sys
import time

import websocket

sys.path.insert(0, ".")
from importlib import import_module

cdp = import_module("cdp-drive".replace("-", "_")) if False else None

# cdp-drive.py 模块名带连字符，用 spec 加载
import importlib.util
spec = importlib.util.spec_from_file_location("cdpdrive", "cdp-drive.py")
cdpdrive = importlib.util.module_from_spec(spec)
spec.loader.exec_module(cdpdrive)


def main():
    ws = websocket.create_connection(cdpdrive.find_page(), timeout=30, suppress_origin=True)

    def dom(expr):
        return cdpdrive.evaluate(ws, expr, await_promise=False, timeout=30)

    # 1. 关键元素存在性 + 旧板块移除
    print("1) DOM 结构:", dom("""(function(){
      var ids=['btn-verify-all','verify-all-status','model-search','model-source','model-only-ok','model-count','models-table','model-page-info','btn-model-prev','btn-model-next'];
      var missing=ids.filter(function(i){return !document.getElementById(i);});
      return JSON.stringify({
        missing: missing,
        thCount: document.querySelectorAll('#models-table th').length,
        oldSectionGone: document.body.innerHTML.indexOf('已发布模型') < 0,
        endpointShown: document.getElementById('endpoint-view').textContent
      });
    })()"""))

    # 2. 表格已渲染：行数=10（分页生效）、页码信息
    time.sleep(4)  # 等一轮 3s 轮询 render
    print("2) 渲染状态:", dom("""(function(){
      var rows=document.querySelectorAll('#models-table tbody tr');
      return JSON.stringify({
        rowCount: rows.length,
        pageInfo: document.getElementById('model-page-info').textContent,
        countText: document.getElementById('model-count').textContent,
        firstRow: rows[0] ? rows[0].innerText.replace(/\\n/g,' | ') : null,
        sourceOptions: document.getElementById('model-source').options.length,
        onlyOkChecked: document.getElementById('model-only-ok').checked
      });
    })()"""))

    # 3. 模拟搜索：输入 glm 后等 debounce，看行数变化
    dom("""(function(){
      var el=document.getElementById('model-search');
      el.value='glm';
      el.dispatchEvent(new Event('input',{bubbles:true}));
      return 'ok';
    })()""")
    time.sleep(1.5)
    print("3) 搜索 glm:", dom("""(function(){
      var rows=document.querySelectorAll('#models-table tbody tr');
      var names=[].slice.call(rows).map(function(r){return r.cells[0].textContent;});
      return JSON.stringify({rowCount:rows.length, names:names.slice(0,12), page:document.getElementById('model-page-info').textContent});
    })()"""))

    # 4. 翻页测试：清空搜索（全量），跳第2页
    dom("""(function(){
      var el=document.getElementById('model-search');
      el.value='';
      el.dispatchEvent(new Event('input',{bubbles:true}));
      return 'ok';
    })()""")
    time.sleep(1.5)
    print("4) 全量:", dom("document.getElementById('model-page-info').textContent"))
    dom("document.getElementById('btn-model-next').click()")
    time.sleep(0.3)
    print("   下一页后:", dom("""(function(){
      return JSON.stringify({page:document.getElementById('model-page-info').textContent,
        prevEnabled:!document.getElementById('btn-model-prev').disabled,
        firstName:document.querySelector('#models-table tbody tr td b').textContent});
    })()"""))
    dom("document.getElementById('btn-model-prev').click()")
    time.sleep(0.3)
    print("   上一页后:", dom("document.getElementById('model-page-info').textContent"))

    # 5. 供应商筛选 + 取消“仅可调用”看未验证/失败是否显示
    dom("""(function(){
      var el=document.getElementById('model-source'); el.value='trae';
      el.dispatchEvent(new Event('change',{bubbles:true}));
      return 'ok';
    })()""")
    time.sleep(0.3)
    print("5) 筛选 trae:", dom("""(function(){
      var rows=document.querySelectorAll('#models-table tbody tr');
      return JSON.stringify({rowCount:rows.length, count:document.getElementById('model-count').textContent,
        page:document.getElementById('model-page-info').textContent});
    })()"""))
    dom("""(function(){
      var el=document.getElementById('model-only-ok'); el.checked=false;
      el.dispatchEvent(new Event('change',{bubbles:true}));
      return 'ok';
    })()""")
    time.sleep(0.3)
    print("   取消仅可调用:", dom("""(function(){
      return JSON.stringify({page:document.getElementById('model-page-info').textContent,
        count:document.getElementById('model-count').textContent});
    })()"""))
    # 复位
    dom("""(function(){
      var el=document.getElementById('model-only-ok'); el.checked=true;
      el.dispatchEvent(new Event('change',{bubbles:true}));
      var s=document.getElementById('model-source'); s.value='';
      s.dispatchEvent(new Event('change',{bubbles:true}));
      return 'ok';
    })()""")

    # 6. 无 JS 报错（最近 console error 捕获不到就跳过；检查 window.__err 钩子未设）
    print("6) render 存活:", dom("typeof render === 'function' ? 'render OK' : 'render MISSING'"))
    return 0


if __name__ == "__main__":
    sys.exit(main())
