/* CreditFerry panel — talks to the Rust core over the Tauri IPC channel. */
(function () {
  'use strict';

  // Tauri v2 injects window.__TAURI_INTERNALS__ (with invoke) into every
  // webview; window.__TAURI__ only exists when the npm api bundle is loaded.
  // Probe the low-level bridge first, then the api-package shapes.
  function invoke(cmd, args) {
    var internals = window.__TAURI_INTERNALS__;
    if (internals && typeof internals.invoke === 'function') {
      return internals.invoke(cmd, args || {}, undefined);
    }
    var t = window.__TAURI__;
    if (t && t.core && typeof t.core.invoke === 'function') {
      return t.core.invoke(cmd, args || {});
    }
    if (t && t.internals && typeof t.internals.invoke === 'function') {
      return t.internals.invoke(cmd, args || {}, undefined);
    }
    return Promise.reject(new Error('Tauri IPC bridge unavailable'));
  }

  function $(id) { return document.getElementById(id); }
  function esc(s) {
    return String(s == null ? '' : s).replace(/[&<>"']/g, function (c) {
      return { '&': '&amp;', '<': '&lt;', '>': '&gt;', '"': '&quot;', "'": '&#39;' }[c];
    });
  }
  function fmtTime(ms) {
    if (!ms) return '—';
    var d = new Date(ms);
    function p(n) { return (n < 10 ? '0' : '') + n; }
    return p(d.getMonth() + 1) + '-' + p(d.getDate()) + ' ' + p(d.getHours()) + ':' + p(d.getMinutes()) + ':' + p(d.getSeconds());
  }
  function fmtNum(n) {
    if (n == null) return '—';
    if (n >= 10000) return (n / 10000).toFixed(1).replace(/\.0$/, '') + ' 万';
    return String(n);
  }
  var toastTimer;
  function toast(msg) {
    var el = $('toast');
    el.textContent = msg;
    el.classList.add('show');
    clearTimeout(toastTimer);
    toastTimer = setTimeout(function () { el.classList.remove('show'); }, 3200);
  }

  var qrPolling = null;

  // ------------------------------------------------------------- rendering

  function probeStat(models) {
    var probed = models.filter(function (m) { return m.probeOk !== null && m.probeOk !== undefined; });
    var ok = probed.filter(function (m) { return m.probeOk === true; }).length;
    return probed.length === 0 ? '—' : (ok + '/' + probed.length);
  }

  function render(snap) {
    $('endpoint-view').textContent = snap.endpoint;
    $('api-key-view').textContent = '••••••••' ;

    // Loomy
    $('loomy-balance').textContent = snap.loomy.balance == null ? '—' : fmtNum(snap.loomy.balance);
    $('loomy-models').textContent = snap.loomy.models.length;
    $('loomy-probe-stat').textContent = probeStat(snap.loomy.models);
    var box = $('loomy-sessions');
    if (!snap.loomy.sessions.length) {
      box.innerHTML = '<div class="muted" style="margin-top:10px">尚未导入会话。若本机装有 Loomy 客户端，点「扫描本机客户端」。</div>';
    } else {
      box.innerHTML = snap.loomy.sessions.map(function (s) {
        var dot = s.expired ? '<span class="dot amber" title="已过期"></span>' : '<span class="dot green"></span>';
        var active = s.active ? ' <span class="badge ok">当前</span>' : '';
        return '<div class="session-item">' + dot +
          '<span><b>' + esc(s.name) + '</b>' + active + '</span>' +
          '<span class="meta">' + esc(s.phone || '') + ' · ' + (s.source === 'client' ? '客户端导入' : '手动') +
          ' · 到期 ' + fmtTime(s.expiresAtMs) + '</span>' +
          '<span style="margin-left:auto;display:flex;gap:6px">' +
          (s.active ? '' : '<button class="small" data-loomy-select="' + esc(s.name) + '">启用</button>') +
          '<button class="small danger" data-loomy-forget="' + esc(s.name) + '">删除</button>' +
          '</span></div>';
      }).join('');
    }

    // Raccoon
    $('raccoon-balance').textContent = snap.raccoon.balance == null ? '—' : fmtNum(snap.raccoon.balance);
    $('raccoon-models').textContent = snap.raccoon.models.length;
    $('raccoon-probe-stat').textContent = probeStat(snap.raccoon.models);
    $('raccoon-status').textContent = snap.raccoon.loggedIn
      ? ('已登录：' + (snap.raccoon.nickname || '（昵称未提供）') + ' · 令牌到期 ' + fmtTime(snap.raccoon.expiresAtMs))
      : '未登录 — 点击「微信扫码登录」';

    // OpenCode
    $('oc-models').textContent = snap.opencode.models.length;
    $('oc-version').textContent = snap.opencode.running ? (snap.opencode.version || '—') : '—';
    $('oc-probe-stat').textContent = probeStat(snap.opencode.models);
    $('oc-status').textContent = snap.opencode.running
      ? ('隔离运行时运行中 · OpenCode ' + (snap.opencode.version || '') + ' · ' + snap.opencode.models.length + ' 个免费模型')
      : '未启动 — 首次启动会自动下载 OpenCode 二进制（约一两分钟）';
    $('btn-oc-start').disabled = !!snap.opencode.running;
    $('btn-oc-start').textContent = snap.opencode.running ? '已运行' : '启动运行时';
    $('btn-oc-stop').disabled = !snap.opencode.running;
    $('btn-oc-refresh').disabled = !snap.opencode.running;

    // Maodao
    var md = snap.maodao || { loggedIn: false, models: [], balance: null };
    $('maodao-balance').textContent = md.balance == null ? '—' : fmtNum(md.balance);
    $('maodao-models').textContent = md.models.length;
    $('maodao-probe-stat').textContent = probeStat(md.models);
    $('maodao-status').textContent = md.loggedIn
      ? ('已登录' + (md.userName ? ('：' + md.userName) : '') + ' · 临时凭证到期 ' + fmtTime(md.expiresAtMs) + ' · 余额为 benefit token 池')
      : '未登录 — 点「浏览器登录」（会自动打开华为登录页），或从本机码道客户端导入';

    // Models
    var kl = snap.kilo || { loggedIn: true, models: [] };
    $('kilo-models').textContent = kl.models.length;
    $('kilo-probe-stat').textContent = probeStat(kl.models);
    $('kilo-status').textContent = '免登录 · ' + kl.models.length + ' 个免费池模型（目录随时可刷新）';
    var zn = snap.zen || { loggedIn: true, models: [], sessions: 0 };
    $('zen-models').textContent = zn.models.length;
    $('zen-probe-stat').textContent = probeStat(zn.models);
    $('zen-sessions').textContent = zn.sessions || 0;
    $('zen-status').textContent = '匿名公共池 · ' + zn.models.length + ' 个免费车道模型 · 会话映射 ' + (zn.sessions || 0) + ' 条';

    // Buddy family (native OAuth: CodeBuddy CN / WorkBuddy Intl)
    var bd = snap.buddy || { loggedIn: false, models: [], nickname: '' };
    $('buddy-models').textContent = bd.models.length;
    $('buddy-probe-stat').textContent = probeStat(bd.models);
    $('buddy-status').textContent = bd.loggedIn
      ? ('已登录' + (bd.nickname ? ('：' + bd.nickname) : '') + ' · 令牌到期 ' + fmtTime(bd.expiresAtMs) + ' · ' + bd.models.length + ' 个模型')
      : '未登录 — 点「浏览器登录」（自动打开官方登录页，完成后自动拉取目录）';
    $('buddy-balance').textContent = (bd.balance != null) ? bd.balance : '—';
    var wb = snap.workbuddy || { loggedIn: false, models: [], nickname: '' };
    $('workbuddy-models').textContent = wb.models.length;
    $('workbuddy-probe-stat').textContent = probeStat(wb.models);
    $('workbuddy-status').textContent = wb.loggedIn
      ? ('已登录' + (wb.nickname ? ('：' + wb.nickname) : '') + ' · 令牌到期 ' + fmtTime(wb.expiresAtMs) + ' · ' + wb.models.length + ' 个模型')
      : '未登录 — 点「浏览器登录」（国际版账号）';
    $('workbuddy-balance').textContent = (wb.balance != null) ? wb.balance : '—';

    // LobsterAI / TRAE / MiniMax (native Bearer-token channels)
    var lb = snap.lobsterai || { loggedIn: false, models: [], nickname: '' };
    $('lobsterai-models').textContent = lb.models.length;
    $('lobsterai-probe-stat').textContent = probeStat(lb.models);
    $('lobsterai-status').textContent = lb.loggedIn
      ? ('已登录' + (lb.nickname ? ('：' + lb.nickname) : '') + ' · 令牌到期 ' + fmtTime(lb.expiresAtMs) + ' · ' + lb.models.length + ' 个模型')
      : '未登录 — 点「浏览器登录」（自动打开有道登录页，完成后自动拉取目录）';
    var tr = snap.trae || { loggedIn: false, models: [], nickname: '' };
    $('trae-models').textContent = tr.models.length;
    $('trae-probe-stat').textContent = probeStat(tr.models);
    $('trae-status').textContent = tr.loggedIn
      ? ('已登录' + (tr.nickname ? ('：' + tr.nickname) : '') + ' · 令牌到期 ' + fmtTime(tr.expiresAtMs) + ' · ' + tr.models.length + ' 个模型')
      : '未登录 — 点「浏览器登录」（自动打开 TRAE 登录页，完成后自动拉取目录）';
    var mm = snap.minimax || { loggedIn: false, models: [] };
    $('minimax-models').textContent = mm.models.length;
    $('minimax-probe-stat').textContent = probeStat(mm.models);
    $('minimax-status').textContent = mm.loggedIn
      ? ('已登录 · 令牌到期 ' + fmtTime(mm.expiresAtMs) + ' · ' + mm.models.length + ' 个模型')
      : '未登录 — 点「浏览器登录」（设备码授权）';
    $('minimax-balance').textContent = (mm.balance != null) ? mm.balance : '—';
    var cl = snap.cline || { loggedIn: false, models: [], nickname: '' };
    $('cline-models').textContent = cl.models.length;
    $('cline-probe-stat').textContent = probeStat(cl.models);
    $('cline-status').textContent = cl.loggedIn
      ? ('已登录' + (cl.nickname ? ('：' + cl.nickname) : '') + ' · 令牌到期 ' + fmtTime(cl.expiresAtMs) + ' · ' + cl.models.length + ' 个模型')
      : '未登录 — 点「浏览器登录」（设备码授权，WorkOS flow）';
    $('cline-balance').textContent = (cl.balance != null) ? cl.balance : '—';
    var qd = snap.qoder || { loggedIn: false, models: [] };
    var qc = snap.qodercn || { loggedIn: false, models: [] };
    var qst = (qd.loggedIn ? '国际版已登录' : '国际版未登录') + ' · ' + (qc.loggedIn ? '中国版已登录' : '中国版未登录')
      + ' · 目录 ' + (qd.models || []).length + '/' + (qc.models || []).length + ' · 推理受 WASM 签名限制（模型不发布）';
    $('qoder-status').textContent = qst;

    // Keepalive (P1-3)
    var ka = snap.keepalive || { lastRunMs: 0, notes: [], blocks: [] };
    $('ka-last').textContent = ka.lastRunMs ? fmtTime(ka.lastRunMs) : '尚未巡检';
    var kaNames = { maodao: '码道', raccoon: '小浣熊', loomy: 'Loomy' };
    $('ka-status').textContent = (ka.notes && ka.notes.length)
      ? ka.notes.map(function (n) { return (kaNames[n.source] || n.source) + '：' + n.note; }).join('　|　')
      : '后台每 ' + (ka.intervalMinutes || 30) + ' 分钟巡检一轮（过期前 ' + (ka.leadMinutes || 60) + ' 分钟窗内才轮换）';
    var blocksEl = $('ka-blocks');
    if (ka.blocks && ka.blocks.length) {
      blocksEl.style.display = 'block';
      blocksEl.innerHTML = '⚠ 终态标记（需重新登录，24 小时后自动重查）：' + ka.blocks.map(function (b) {
        return '<b>' + (kaNames[b.source] || b.source) + '</b> ' + esc(b.reason) + '（' + fmtTime(b.atMs) + '）';
      }).join('；');
    } else {
      blocksEl.style.display = 'none';
      blocksEl.innerHTML = '';
    }
    var all = snap.loomy.models.concat(snap.raccoon.models).concat(snap.opencode.models).concat(md.models).concat(kl.models).concat(zn.models).concat(bd.models).concat(wb.models).concat(lb.models).concat(tr.models).concat(mm.models).concat(cl.models);
    $('model-count').textContent = all.length;
    var tbody = document.querySelector('#models-table tbody');
    if (!all.length) {
      tbody.innerHTML = '<tr><td colspan="8" class="muted">暂无模型 — 请先登录上游并刷新</td></tr>';
    } else {
      tbody.innerHTML = all.map(function (m) {
        var mult = m.multiplier == null ? '—' : ('×' + m.multiplier);
        var status = (m.source === 'kilo' || m.source === 'zen') ? '<span class="badge free">免费池</span>'
          : m.billingStatus === 'limited_free' ? '<span class="badge free">限免</span>'
          : m.billingStatus === 'discount' ? '<span class="badge promo">折扣</span>'
          : '<span class="badge no">标准</span>';
        var src = m.source === 'raccoon' ? '<span class="badge raccoon">小浣熊</span>'
          : m.source === 'opencode' ? '<span class="badge opencode">OpenCode</span>'
          : m.source === 'maodao' ? '<span class="badge maodao">码道</span>'
          : m.source === 'kilo' ? '<span class="badge kilo">Kilo</span>'
          : m.source === 'zen' ? '<span class="badge zen">Zen</span>'
          : m.source === 'buddy' ? '<span class="badge buddy">CodeBuddy</span>'
          : m.source === 'workbuddy' ? '<span class="badge workbuddy">WorkBuddy</span>'
          : m.source === 'lobsterai' ? '<span class="badge lobsterai">LobsterAI</span>'
          : m.source === 'trae' ? '<span class="badge trae">TRAE</span>'
          : m.source === 'minimax' ? '<span class="badge minimax">MiniMax</span>'
          : m.source === 'cline' ? '<span class="badge cline">Cline</span>'
          : '<span class="badge loomy">Loomy</span>';
        var probe;
        if (m.probeOk === true) {
          probe = '<span class="badge ok" title="' + esc(m.probeNote || '') + '">✓ 可调用</span>';
        } else if (m.probeOk === false) {
          probe = '<span class="badge no" style="color:#b91c1c;border-color:#fecaca;background:#fef2f2" title="' + esc(m.probeNote || '') + '">✗ ' + esc(m.probeNote || '不可用') + '</span>';
        } else {
          probe = '<span class="badge no" title="尚未验证">未验证</span>';
        }
        var rowStyle = (m.probeOk === false) ? ' style="opacity:.55"' : '';
        return '<tr' + rowStyle + '><td><b>' + esc(m.clientId) + '</b></td>' +
          '<td>' + src + '</td>' +
          '<td><span class="badge ok">✓</span></td>' +
          '<td>' + (m.vision ? '<span class="badge ok">✓</span>' : '<span class="badge no">—</span>') + '</td>' +
          '<td>' + fmtNum(m.context) + '</td><td>' + fmtNum(m.maxOutput) + '</td>' +
          '<td>' + mult + '</td><td>' + status + '</td><td>' + probe + '</td></tr>';
      }).join('');
    }

    // Ledger
    $('ledger-loomy').textContent = snap.loomy.balance == null ? '—' : fmtNum(snap.loomy.balance);
    $('ledger-raccoon').textContent = snap.raccoon.balance == null ? '—' : fmtNum(snap.raccoon.balance);
    $('ledger-maodao').textContent = (snap.maodao && snap.maodao.balance != null) ? fmtNum(snap.maodao.balance) : '—';
    var lbody = document.querySelector('#ledger-table tbody');
    if (!snap.ledger.length) {
      lbody.innerHTML = '<tr><td colspan="6" class="muted">暂无记录</td></tr>';
    } else {
      lbody.innerHTML = snap.ledger.map(function (e) {
        var src = e.source === 'raccoon' ? '<span class="badge raccoon">小浣熊</span>'
          : e.source === 'opencode' ? '<span class="badge opencode">OpenCode</span>'
          : e.source === 'maodao' ? '<span class="badge maodao">码道</span>'
          : e.source === 'kilo' ? '<span class="badge kilo">Kilo</span>'
          : e.source === 'zen' ? '<span class="badge zen">Zen</span>'
          : e.source === 'buddy' ? '<span class="badge buddy">CodeBuddy</span>'
          : e.source === 'workbuddy' ? '<span class="badge workbuddy">WorkBuddy</span>'
          : e.source === 'lobsterai' ? '<span class="badge lobsterai">LobsterAI</span>'
          : e.source === 'trae' ? '<span class="badge trae">TRAE</span>'
          : e.source === 'minimax' ? '<span class="badge minimax">MiniMax</span>'
          : e.source === 'cline' ? '<span class="badge cline">Cline</span>'
          : '<span class="badge loomy">Loomy</span>';
        var points = e.points == null ? '—' : e.points;
        var toks = (e.promptTokens == null ? '—' : e.promptTokens) + ' / ' + (e.completionTokens == null ? '—' : e.completionTokens);
        var kind = e.kind === 'reported' ? '<span class="badge ok">已扣分</span>' : '<span class="badge no">仅 token</span>';
        return '<tr><td>' + fmtTime(e.tsMs) + '</td><td>' + src + '</td><td>' + esc(e.model) + '</td>' +
          '<td>' + points + '</td><td>' + toks + '</td><td>' + kind + '</td></tr>';
      }).join('');
    }
  }

  function refresh() {
    invoke('get_snapshot').then(render).catch(function (e) { toast('状态读取失败：' + e); });
    invoke('server_status').then(function (s) {
      var pill = $('server-pill');
      pill.textContent = s.running ? ('服务运行中 :' + s.port) : '服务未启动';
      pill.className = 'pill ' + (s.running ? 'on' : 'off');
    }).catch(function () {});
  }

  // ------------------------------------------------------------- actions

  $('btn-loomy-import').addEventListener('click', function () {
    invoke('loomy_import_client').then(function (r) {
      toast(r.added ? ('发现并导入 ' + r.added + ' 个会话') : '未发现新的客户端会话');
      refresh();
    }).catch(function (e) { toast(e); });
  });

  $('btn-loomy-refresh').addEventListener('click', function () {
    invoke('loomy_refresh').then(function (r) {
      toast('Loomy：' + r.models + ' 个模型' + (r.balance != null ? '，余额 ' + r.balance : ''));
      refresh();
    }).catch(function (e) { toast(e); });
  });

  $('btn-loomy-manual').addEventListener('click', function () {
    var value = $('loomy-manual').value.trim();
    if (!value) return toast('先粘贴会话 token');
    invoke('loomy_set_session', { session: value }).then(function () {
      $('loomy-manual').value = '';
      toast('会话已保存并设为当前');
      refresh();
    }).catch(function (e) { toast(e); });
  });

  document.addEventListener('click', function (ev) {
    var select = ev.target.getAttribute && ev.target.getAttribute('data-loomy-select');
    var forget = ev.target.getAttribute && ev.target.getAttribute('data-loomy-forget');
    if (select) {
      invoke('loomy_select', { name: select }).then(refresh).catch(function (e) { toast(e); });
    } else if (forget) {
      invoke('loomy_forget', { name: forget }).then(refresh).catch(function (e) { toast(e); });
    }
  });

  $('btn-raccoon-qr').addEventListener('click', function () {
    invoke('raccoon_qr_start').then(function (r) {
      $('raccoon-qr').classList.remove('hidden');
      $('qr-svg').innerHTML = r.svg;
      $('qr-url').textContent = r.scanUrl;
      $('qr-url').href = r.scanUrl;
      $('qr-hint').textContent = '等待扫码…（约 2 秒刷新一次状态）';
      if (qrPolling) clearInterval(qrPolling);
      qrPolling = setInterval(pollQr, 2000);
    }).catch(function (e) { toast(e); });
  });

  function pollQr() {
    invoke('raccoon_qr_status').then(function (r) {
      if (r.status === 'pending') {
        $('qr-hint').textContent = '等待扫码…';
      } else if (r.status === 'logging') {
        $('qr-hint').textContent = '已扫码，请在手机上确认';
      } else if (r.status === 'success') {
        clearInterval(qrPolling); qrPolling = null;
        $('raccoon-qr').classList.add('hidden');
        toast('小浣熊登录成功');
        refresh();
      } else if (r.status === 'canceled') {
        clearInterval(qrPolling); qrPolling = null;
        $('qr-hint').textContent = '扫码被取消，可重新发起';
      } else if (r.status === 'timed_out') {
        clearInterval(qrPolling); qrPolling = null;
        $('qr-hint').textContent = '二维码超时（5 分钟），请重新发起';
      }
    }).catch(function () {});
  }

  $('btn-raccoon-refresh').addEventListener('click', function () {
    invoke('raccoon_refresh').then(function (r) {
      toast('小浣熊：' + r.models + ' 个模型' + (r.balance != null ? '，余额 ' + r.balance : ''));
      refresh();
    }).catch(function (e) { toast(e); });
  });

  $('btn-raccoon-logout').addEventListener('click', function () {
    invoke('raccoon_logout').then(function () {
      toast('已退出小浣熊');
      refresh();
    }).catch(function (e) { toast(e); });
  });

  $('btn-oc-start').addEventListener('click', function () {
    var btn = $('btn-oc-start');
    btn.disabled = true;
    btn.textContent = '启动中…';
    $('oc-status').textContent = '正在准备隔离运行时（首次需下载二进制）…';
    invoke('opencode_start').then(function (r) {
      btn.disabled = false;
      btn.textContent = '启动运行时';
      toast('OpenCode 运行时已启动' + (r.version ? ('（' + r.version + '）') : ''));
      refresh();
    }).catch(function (e) {
      btn.disabled = false;
      btn.textContent = '启动运行时';
      $('oc-status').textContent = '启动失败：' + e;
      toast('启动失败：' + e);
    });
  });

  $('btn-oc-stop').addEventListener('click', function () {
    invoke('opencode_stop').then(function () {
      toast('OpenCode 运行时已停止');
      refresh();
    }).catch(function (e) { toast(e); });
  });

  $('btn-oc-refresh').addEventListener('click', function () {
    invoke('opencode_refresh').then(function (r) {
      toast('OpenCode：' + r.models + ' 个免费模型');
      refresh();
    }).catch(function (e) { toast(e); });
  });

  // ---- 码道：浏览器登录 / 客户端导入 / 刷新 / 退出 ----
  var mdPolling = null;
  $('btn-maodao-login').addEventListener('click', function () {
    invoke('maodao_login_start').then(function (r) {
      $('maodao-login-hint').classList.remove('hidden');
      $('maodao-login-hint').textContent = '已打开浏览器，请在华为登录页完成授权（约 5 分钟内有效）… 若浏览器没有自动打开，请手动访问：' + r.url;
      if (mdPolling) clearInterval(mdPolling);
      mdPolling = setInterval(pollMaodaoLogin, 1500);
    }).catch(function (e) { toast('登录发起失败：' + e); });
  });
  function pollMaodaoLogin() {
    invoke('maodao_login_status').then(function (r) {
      if (r.status === 'pending') return;
      if (mdPolling) { clearInterval(mdPolling); mdPolling = null; }
      $('maodao-login-hint').classList.add('hidden');
      if (r.status === 'success') {
        toast('码道登录成功' + (r.warning ? ('（模型刷新：' + r.warning + '）') : ''));
      } else if (r.status === 'failed') {
        toast('码道登录失败：' + r.message);
      } else if (r.status === 'timed_out') {
        toast('码道登录超时，请重新发起');
      }
      refresh();
    }).catch(function () {});
  }
  $('btn-maodao-import').addEventListener('click', function () {
    invoke('maodao_import_client').then(function (r) {
      toast('码道：已导入客户端会话，' + r.models + ' 个模型');
      refresh();
    }).catch(function (e) { toast(e); });
  });
  $('btn-maodao-refresh').addEventListener('click', function () {
    invoke('maodao_refresh').then(function (r) {
      toast('码道：' + r.models + ' 个模型' + (r.balance != null ? '，token 余额 ' + r.balance : ''));
      refresh();
    }).catch(function (e) { toast(e); });
  });
  $('btn-maodao-logout').addEventListener('click', function () {
    invoke('maodao_logout').then(function () {
      toast('已退出码道（不影响桌面客户端登录）');
      refresh();
    }).catch(function (e) { toast(e); });
  });

  // ---- Kilo：免登录车道 ----
  $('btn-kilo-refresh').addEventListener('click', function () {
    var btn = $('btn-kilo-refresh');
    btn.disabled = true;
    btn.textContent = '刷新中…';
    invoke('kilo_refresh').then(function (r) {
      btn.disabled = false;
      btn.textContent = '刷新模型';
      toast('Kilo：' + r.models + ' 个免费池模型');
      refresh();
    }).catch(function (e) {
      btn.disabled = false;
      btn.textContent = '刷新模型';
      toast(e);
    });
  });

  // ---- Zen：OpenCode 匿名公共车道 ----
  $('btn-zen-refresh').addEventListener('click', function () {
    var btn = $('btn-zen-refresh');
    btn.disabled = true;
    btn.textContent = '刷新中…';
    invoke('zen_refresh').then(function (r) {
      btn.disabled = false;
      btn.textContent = '刷新模型';
      toast('Zen：' + r.models + ' 个免费车道模型');
      refresh();
    }).catch(function (e) {
      btn.disabled = false;
      btn.textContent = '刷新模型';
      toast(e);
    });
  });

  // ---- Buddy family (native OAuth/login + refresh + probe) ----
  function ensureLoginHint(source) {
    var id = 'login-hint-' + source;
    var el = document.getElementById(id);
    if (!el) {
      el = document.createElement('div');
      el.id = id;
      el.className = 'muted hidden';
      el.style.marginTop = '6px';
      var st = document.getElementById(source + '-status');
      if (st && st.parentNode) st.parentNode.insertBefore(el, st);
    }
    return el;
  }
  function buddyLogin(source, btn) {
    var cmd = source === 'workbuddy' ? 'workbuddy_login'
      : source === 'minimax' ? 'minimax_login'
      : source === 'cline' ? 'cline_login'
      : source === 'qoder' ? 'qoder_login'
      : source === 'qodercn' ? 'qodercn_login'
      : source === 'lobsterai' ? 'lobsterai_login'
      : source === 'trae' ? 'trae_login'
      : 'buddy_login';
    if (btn) { btn.disabled = true; btn.textContent = '打开登录页…'; }
    invoke(cmd).then(function (r) {
      if (btn) { btn.disabled = false; btn.textContent = '浏览器登录'; }
      var authUrl = (r && r.authUrl) || '';
      var userCode = (r && r.userCode) || '';
      var hint = ensureLoginHint(source);
      hint.classList.remove('hidden');
      var waited = 0;
      function showHint() {
        var msg = '请在浏览器中完成授权…（已等待 ' + Math.round(waited) + 's / 最多 300s）';
        if (userCode) msg += ' · 设备码 <b>' + userCode + '</b>';
        if (authUrl) msg += ' · 若未自动打开可手动访问：<a href="' + authUrl + '" target="_blank" rel="noopener">' + authUrl + '</a>';
        hint.innerHTML = msg;
      }
      showHint();
      var iv = setInterval(function () {
        waited += 1.5;
        invoke('get_snapshot').then(function (s) {
          render(s);
          var ch = s[source] || {};
          if (ch.loggedIn) {
            clearInterval(iv);
            hint.classList.add('hidden');
            var label = (source === 'qoder') ? 'Qoder 国际版' : (source === 'qodercn') ? 'Qoder 中国版' : source;
            toast(label + ' 登录成功' + ((r && r.gated) ? '（推理受 WASM 限制，模型仅供查看）' : ''));
            refresh();
          } else if (waited >= 300) {
            clearInterval(iv);
            hint.innerHTML = '登录超时：未在 5 分钟内完成浏览器授权。请重新点「浏览器登录」，并在打开的页面里完成授权。';
          } else {
            showHint();
          }
        }).catch(function () {});
      }, 1500);
    }).catch(function (e) {
      if (btn) { btn.disabled = false; btn.textContent = '浏览器登录'; }
      toast(e);
    });
  }
  function buddyRefresh(source, btn) {
    var cmd = source === 'workbuddy' ? 'workbuddy_refresh' : 'buddy_refresh';
    btn.disabled = true;
    btn.textContent = '刷新中…';
    invoke(cmd).then(function (r) {
      btn.disabled = false;
      btn.textContent = '刷新模型';
      toast(source + '：' + r.models + ' 个模型');
      refresh();
    }).catch(function (e) {
      btn.disabled = false;
      btn.textContent = '刷新模型';
      toast(e);
    });
  }
  $('btn-buddy-login').addEventListener('click', function () { buddyLogin('buddy', this); });
  $('btn-buddy-refresh').addEventListener('click', function () { buddyRefresh('buddy', this); });
  $('btn-buddy-probe').addEventListener('click', function () { probeClick('buddy', this); });
  $('btn-workbuddy-login').addEventListener('click', function () { buddyLogin('workbuddy', this); });
  $('btn-workbuddy-refresh').addEventListener('click', function () { buddyRefresh('workbuddy', this); });
  $('btn-workbuddy-probe').addEventListener('click', function () { probeClick('workbuddy', this); });

  // ---- LobsterAI / TRAE / MiniMax (native Bearer-token channels) ----
  function genericRefresh(source, btn) {
    btn.disabled = true;
    btn.textContent = '刷新中…';
    invoke(source + '_refresh').then(function (r) {
      btn.disabled = false;
      btn.textContent = '刷新模型';
      toast(source + '：' + r.models + ' 个模型');
      refresh();
    }).catch(function (e) {
      btn.disabled = false;
      btn.textContent = '刷新模型';
      toast(e);
    });
  }
  $('btn-lobsterai-refresh').addEventListener('click', function () { genericRefresh('lobsterai', this); });
  $('btn-lobsterai-probe').addEventListener('click', function () { probeClick('lobsterai', this); });
  $('btn-lobsterai-login').addEventListener('click', function () { buddyLogin('lobsterai', this); });
  $('btn-trae-refresh').addEventListener('click', function () { genericRefresh('trae', this); });
  $('btn-trae-probe').addEventListener('click', function () { probeClick('trae', this); });
  $('btn-trae-login').addEventListener('click', function () { buddyLogin('trae', this); });
  $('btn-minimax-refresh').addEventListener('click', function () { genericRefresh('minimax', this); });
  $('btn-minimax-probe').addEventListener('click', function () { probeClick('minimax', this); });
  $('btn-minimax-login').addEventListener('click', function () { buddyLogin('minimax', this); });
  $('btn-cline-refresh').addEventListener('click', function () { genericRefresh('cline', this); });
  $('btn-cline-probe').addEventListener('click', function () { probeClick('cline', this); });
  $('btn-cline-login').addEventListener('click', function () { buddyLogin('cline', this); });
  $('btn-qoder-login').addEventListener('click', function () { buddyLogin('qoder', this); });
  $('btn-qodercn-login').addEventListener('click', function () { buddyLogin('qodercn', this); });

  // ---- 验证可调用性（三源通用） ----
  function probeClick(source, btn) {
    btn.disabled = true;
    var old = btn.textContent;
    btn.textContent = '验证中…';
    invoke('probe_models', { source: source }).then(function (r) {
      btn.disabled = false;
      btn.textContent = old;
      toast(source + '：已验证 ' + r.probed + ' 个，可用 ' + r.ok + '，不可用 ' + r.failed);
      refresh();
    }).catch(function (e) {
      btn.disabled = false;
      btn.textContent = old;
      toast('验证失败：' + e);
    });
  }
  $('btn-loomy-probe').addEventListener('click', function () { probeClick('loomy', this); });
  $('btn-raccoon-probe').addEventListener('click', function () { probeClick('raccoon', this); });
  $('btn-oc-probe').addEventListener('click', function () { probeClick('opencode', this); });
  $('btn-maodao-probe').addEventListener('click', function () { probeClick('maodao', this); });
  $('btn-kilo-probe').addEventListener('click', function () { probeClick('kilo', this); });
  $('btn-zen-probe').addEventListener('click', function () { probeClick('zen', this); });

  // ---- 账号保活：立即巡检一轮 ----
  $('btn-keepalive-run').addEventListener('click', function () {
    var btn = $('btn-keepalive-run');
    btn.disabled = true;
    btn.textContent = '巡检中…';
    invoke('keepalive_run_once').then(function () {
      btn.disabled = false;
      btn.textContent = '立即巡检一轮';
      toast('保活巡检完成');
      refresh();
    }).catch(function (e) {
      btn.disabled = false;
      btn.textContent = '立即巡检一轮';
      toast('巡检失败：' + e);
    });
  });

  $('btn-export-wb').addEventListener('click', function () {
    invoke('export_workbuddy').then(function (r) {
      $('wb-result').textContent = (r.changed ? '已写入 ' : '无需变更 ') + r.count + ' 条 → ' + r.path;
      toast(r.changed ? '已写入 WorkBuddy 配置（原文件有备份）' : '配置已是最新');
      refresh();
    }).catch(function (e) { toast('写入失败：' + e); });
  });

  $('btn-clean-wb').addEventListener('click', function () {
    invoke('cleanup_workbuddy').then(function (r) {
      $('wb-result').textContent = r.changed ? ('已摘除 ' + r.count + ' 条') : '没有本工具写入的条目';
      toast(r.changed ? '已摘除（有备份）' : '无需摘除');
      refresh();
    }).catch(function (e) { toast(e); });
  });

  $('btn-export-wb-intl').addEventListener('click', function () {
    invoke('export_workbuddy_intl').then(function (r) {
      $('wb-intl-result').textContent = (r.changed ? '已写入 ' : '无需变更 ') + r.count + ' 条 → ' + r.path;
      toast(r.changed ? '已写入 WorkBuddy 国际版配置（原文件有备份）' : '配置已是最新');
      refresh();
    }).catch(function (e) { toast('写入失败：' + e); });
  });

  $('btn-clean-wb-intl').addEventListener('click', function () {
    invoke('cleanup_workbuddy_intl').then(function (r) {
      $('wb-intl-result').textContent = r.changed ? ('已摘除 ' + r.count + ' 条') : '没有本工具写入的条目';
      toast(r.changed ? '已摘除（有备份）' : '无需摘除');
      refresh();
    }).catch(function (e) { toast(e); });
  });

  $('btn-export-qoder').addEventListener('click', function () {
    invoke('export_qoder').then(function (r) {
      $('qoder-result').textContent = (r.changed ? '已写入 ' : '无需变更 ') + r.count + ' 个模型 → ' + r.path
        + (r.providerId ? '（provider: ' + r.providerId + '）' : '');
      toast(r.changed ? '已写入 Qoder CN providers（原文件有备份），重启 Qoder CN 生效' : '配置已是最新');
      refresh();
    }).catch(function (e) { toast('写入失败：' + e); });
  });

  $('btn-clean-qoder').addEventListener('click', function () {
    invoke('cleanup_qoder').then(function (r) {
      $('qoder-result').textContent = r.changed ? ('已摘除 ' + r.count + ' 个 provider') : '没有本工具写入的 provider';
      toast(r.changed ? '已摘除（有备份）' : '无需摘除');
      refresh();
    }).catch(function (e) { toast(e); });
  });

  $('btn-export-qoder-intl').addEventListener('click', function () {
    invoke('export_qoder_intl').then(function (r) {
      $('qoder-intl-result').textContent = (r.changed ? '已写入 ' : '无需变更 ') + r.count + ' 个模型 → ' + r.path
        + (r.providerId ? '（provider: ' + r.providerId + '）' : '');
      toast(r.changed ? '已写入 Qoder 国际版 providers（原文件有备份），重启 Qoder 生效' : '配置已是最新');
      refresh();
    }).catch(function (e) { toast('写入失败：' + e); });
  });

  $('btn-clean-qoder-intl').addEventListener('click', function () {
    invoke('cleanup_qoder_intl').then(function (r) {
      $('qoder-intl-result').textContent = r.changed ? ('已摘除 ' + r.count + ' 个 provider') : '没有本工具写入的 provider';
      toast(r.changed ? '已摘除（有备份）' : '无需摘除');
      refresh();
    }).catch(function (e) { toast(e); });
  });

  $('btn-export-dsh').addEventListener('click', function () {
    invoke('export_dsh').then(function (r) {
      $('dsh-result').textContent = '已写入 ' + r.count + ' 个模型 → ' + r.path + '（密钥已写入 ~/.dsh/.credentials.yaml）';
      toast('DeepSeek Harness 已配置，重启后在模型选择器选 CreditFerry');
      refresh();
    }).catch(function (e) { toast('配置失败：' + e); });
  });

  $('btn-clean-dsh').addEventListener('click', function () {
    invoke('cleanup_dsh').then(function (r) {
      $('dsh-result').textContent = r.changed ? ('已移除 ' + r.count + ' 个模型条目与密钥') : '没有本工具写入的配置';
      toast(r.changed ? '已移除（有备份）' : '无需移除');
      refresh();
    }).catch(function (e) { toast(e); });
  });

  function copyText(text, okMsg) {
    if (navigator.clipboard && navigator.clipboard.writeText) {
      navigator.clipboard.writeText(text).then(function () { toast(okMsg); });
    } else {
      var ta = document.createElement('textarea');
      ta.value = text; document.body.appendChild(ta); ta.select();
      document.execCommand('copy'); document.body.removeChild(ta);
      toast(okMsg);
    }
  }
  $('btn-copy-key').addEventListener('click', function () {
    invoke('get_api_key').then(function (k) { copyText(k, '入口密钥已复制'); });
  });

  // ------------------------------------------------- DSH connector platforms

  var ACTION_LABEL = {
    install: '安装并启用',
    upgrade: '升级到匹配版本',
    enable: '启用',
    none: '',
    unsupported: ''
  };

  function wbLine(v) {
    if (!v) return '';
    var c = v.catalog || {};
    return (v.desktopAuth ? '桌面 App 已登录' : '桌面 App 未登录')
      + ' · 插件自留副本' + (v.ownAuth ? '有' : '无')
      + ' · 目录 ' + (c.present ? c.models + ' 个（' + (c.source || '?') + '，' + fmtTime(c.fetchedAtMs) + '）' : '空');
  }

  function qoderLine(v) {
    if (!v) return '';
    var c = v.catalog || {};
    return (v.patSaved ? 'PAT 已存（' + (v.region || '?') + '，' + fmtTime(v.savedAtMs) + '）' : '未配 PAT')
      + ' · 目录 ' + (c.present ? c.models + ' 个（' + (c.source || '?') + '）' : '空');
  }

  function platformExtra(key, entry, doc) {
    if (key === 'workbuddy') {
      var home = doc.workbuddyHome || {};
      return '国内版：' + wbLine(home.cn) + '；国际版：' + wbLine(home.intl);
    }
    var st = entry.state || {};
    if (!st.present) return '该 profile 还没有这个插件的数据目录（装好后在 DSH 卡片里粘 PAT）';
    var ci = (st.checkin || {}).cn || {};
    return '国内版：' + qoderLine(st.cn) + '；国际版：' + qoderLine(st.intl)
      + ' · 签到 ' + (ci.lastDate || '未签') + (ci.amount ? '（+' + ci.amount + '）' : '')
      + ' · 自动签到 ' + ((st.autoCheckIn || {}).cn ? '开' : '关')
      + ' · 侧栏额度 ' + ((st.sidebarQuota || {}).cn ? '开' : '关');
  }

  function renderConnectors(doc) {
    var box = $('connectors');
    if (!doc.found) {
      $('connectors-hint').textContent = '没找到 ~/.dsh，请先安装并启动一次 DeepSeek Harness';
      box.innerHTML = '';
      return;
    }
    $('connectors-hint').textContent =
      '~/.dsh · 桌面内核 ' + (doc.runtimeKernel || '未知')
      + ' · pnpm ' + ({ bundled: '用 DSH 自带', external: '用系统', none: '不可用' }[doc.pnpm.kind] || doc.pnpm.kind);
    var html = '';
    (doc.profiles || []).forEach(function (p) {
      (p.platforms || []).forEach(function (e) {
        var label = ACTION_LABEL[e.action] || '';
        var busy = e.action === 'none' || e.action === 'unsupported';
        html += '<div class="row" style="margin-top:10px;align-items:flex-start">'
          + '<div style="flex:1">'
          + '<b>' + esc(e.title) + '</b> <span class="muted">profile ' + esc(p.name)
          + ' · 内核 ' + esc(p.kernel || '未知') + ' · ' + esc(e.package) + '</span><br>'
          + '<span class="muted">' + esc(e.doc) + '</span><br>'
          + '<span class="muted">已装 ' + esc(e.installed || '无') + ' · ' + (e.enabled ? '已启用' : '未启用')
          + ' · 推荐 ' + esc(e.recommended || '无匹配') + ' — ' + esc(e.note) + '</span><br>'
          + '<span class="muted">' + esc(platformExtra(e.key, e, doc)) + '</span>'
          + '</div><div style="min-width:210px;text-align:right">'
          + (label ? '<button class="small" data-plat="' + esc(e.key) + '" data-profile="' + esc(p.name)
              + '" data-action="' + esc(e.action) + '">' + esc(label) + '</button>' : '')
          + (e.installed ? ' <button class="small" data-plat="' + esc(e.key) + '" data-profile="' + esc(p.name)
              + '" data-action="disable">停用</button> <button class="small" data-plat="' + esc(e.key)
              + '" data-profile="' + esc(p.name) + '" data-action="uninstall">卸载</button>' : '')
          + (busy && e.action === 'unsupported' ? '<div class="muted">升级 DSH 内核后再装</div>' : '')
          + '</div></div>';
      });
    });
    box.innerHTML = html || '<div class="muted">没有可用的 DSH profile</div>';
    Array.prototype.forEach.call(box.querySelectorAll('button[data-action]'), function (btn) {
      btn.addEventListener('click', function () {
        var args = { platform: btn.dataset.plat, profile: btn.dataset.profile, action: btn.dataset.action };
        btn.disabled = true;
        btn.textContent = '执行中…';
        invoke('dsh_connect_apply', args).then(function (r) {
          toast(r.changed ? '已完成，重启 DSH 生效' : '没有需要改动的');
          $('connectors-hint').textContent = (r.log || []).join('\n');
          loadConnectors();
        }).catch(function (err) {
          toast('失败：' + err);
          loadConnectors();
        });
      });
    });
  }

  function loadConnectors() {
    invoke('dsh_connect_status').then(renderConnectors).catch(function (e) {
      $('connectors-hint').textContent = '读取失败：' + e;
    });
  }

  $('btn-connectors-refresh').addEventListener('click', loadConnectors);

  // boot
  refresh();
  loadConnectors();
  setInterval(refresh, 3000);
})();
