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
    var all = snap.loomy.models.concat(snap.raccoon.models).concat(snap.opencode.models).concat(md.models);
    $('model-count').textContent = all.length;
    var tbody = document.querySelector('#models-table tbody');
    if (!all.length) {
      tbody.innerHTML = '<tr><td colspan="8" class="muted">暂无模型 — 请先登录上游并刷新</td></tr>';
    } else {
      tbody.innerHTML = all.map(function (m) {
        var mult = m.multiplier == null ? '—' : ('×' + m.multiplier);
        var status = m.billingStatus === 'limited_free' ? '<span class="badge free">限免</span>'
          : m.billingStatus === 'discount' ? '<span class="badge promo">折扣</span>'
          : '<span class="badge no">标准</span>';
        var src = m.source === 'raccoon' ? '<span class="badge raccoon">小浣熊</span>'
          : m.source === 'opencode' ? '<span class="badge opencode">OpenCode</span>'
          : m.source === 'maodao' ? '<span class="badge maodao">码道</span>'
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
