#!/usr/bin/env python3
"""CDP 验证积分账本翻页：DOM 元素、每页 10 条、上下页、边界禁用、总条数。"""
import json
import importlib.util
import sys
import time

import websocket

spec = importlib.util.spec_from_file_location("cdpdrive", "cdp-drive.py")
cdpdrive = importlib.util.module_from_spec(spec)
spec.loader.exec_module(cdpdrive)


def main():
    ws = websocket.create_connection(cdpdrive.find_page(), timeout=30, suppress_origin=True)

    def dom(expr):
        return cdpdrive.evaluate(ws, expr, await_promise=False, timeout=30)

    # 等首轮 render
    time.sleep(4)

    print("1) 账本 DOM:", dom("""(function(){
      var ids=['ledger-table','btn-ledger-prev','btn-ledger-next','ledger-page-info','ledger-count'];
      var missing=ids.filter(function(i){return !document.getElementById(i);});
      return JSON.stringify({missing:missing});
    })()"""))

    print("2) 首页状态:", dom("""(function(){
      var rows=document.querySelectorAll('#ledger-table tbody tr');
      return JSON.stringify({
        rowCount: rows.length,
        pageInfo: document.getElementById('ledger-page-info').textContent,
        countText: document.getElementById('ledger-count').textContent,
        prevDisabled: document.getElementById('btn-ledger-prev').disabled,
        nextDisabled: document.getElementById('btn-ledger-next').disabled,
        firstTime: rows[0] ? rows[0].cells[0].textContent : null,
        lastTime: rows[rows.length-1] ? rows[rows.length-1].cells[0].textContent : null
      });
    })()"""))

    # 翻到第 2 页
    dom("document.getElementById('btn-ledger-next').click()")
    time.sleep(0.3)
    print("3) 下一页:", dom("""(function(){
      var rows=document.querySelectorAll('#ledger-table tbody tr');
      return JSON.stringify({
        pageInfo: document.getElementById('ledger-page-info').textContent,
        prevDisabled: document.getElementById('btn-ledger-prev').disabled,
        firstTime: rows[0] ? rows[0].cells[0].textContent : null,
        rowCount: rows.length
      });
    })()"""))

    # 翻回第 1 页
    dom("document.getElementById('btn-ledger-prev').click()")
    time.sleep(0.3)
    print("4) 上一页回第1页:", dom("""(function(){
      return JSON.stringify({
        pageInfo: document.getElementById('ledger-page-info').textContent,
        prevDisabled: document.getElementById('btn-ledger-prev').disabled
      });
    })()"""))

    # 连续点下一页直到最后一页，检查边界禁用
    dom("""(function(){
      for(var i=0;i<40;i++){ var b=document.getElementById('btn-ledger-next'); if(!b.disabled) b.click(); else break; }
      return 'ok';
    })()""")
    time.sleep(0.4)
    print("5) 末页边界:", dom("""(function(){
      var rows=document.querySelectorAll('#ledger-table tbody tr');
      return JSON.stringify({
        pageInfo: document.getElementById('ledger-page-info').textContent,
        nextDisabled: document.getElementById('btn-ledger-next').disabled,
        prevDisabled: document.getElementById('btn-ledger-prev').disabled,
        rowCount: rows.length
      });
    })()"""))

    return 0


if __name__ == "__main__":
    sys.exit(main())
