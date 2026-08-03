/* caguastore home — clock, SW, instant search (apps + CC tasks), pulse feed,
   quick prompt to hermes (`>` mode). Dependency-free (repo rule: no npm/CDN). */
(function () {
  'use strict';

  var HERMES_URL = 'https://caguaserver.tail82f3c6.ts.net:8796';
  function hermesChat(name) { return HERMES_URL + '/#c/' + encodeURIComponent(name); }

  // ── clock ──────────────────────────────────────────────────────────────────
  var clock = document.getElementById('clock');
  var date = document.getElementById('date');
  var days = ['sun', 'mon', 'tue', 'wed', 'thu', 'fri', 'sat'];
  var months = ['jan', 'feb', 'mar', 'apr', 'may', 'jun', 'jul', 'aug', 'sep', 'oct', 'nov', 'dec'];
  function greeting(h) {
    if (h < 6) return 'a dormir';
    if (h < 12) return 'buenos días';
    if (h < 20) return 'buenas tardes';
    return 'buenas noches';
  }
  function tick() {
    var d = new Date();
    clock.textContent =
      String(d.getHours()).padStart(2, '0') + ':' + String(d.getMinutes()).padStart(2, '0');
    date.textContent = days[d.getDay()] + ' ' + d.getDate() + ' ' + months[d.getMonth()] +
      ' · ' + greeting(d.getHours());
  }
  tick();
  setInterval(tick, 15000);
  if ('serviceWorker' in navigator) {
    navigator.serviceWorker.register('/sw.js').catch(function () {});
  }

  // ── helpers ────────────────────────────────────────────────────────────────
  function getJSON(url) {
    return fetch(url).then(function (r) {
      if (!r.ok) throw new Error(url + ' -> ' + r.status);
      return r.json();
    });
  }
  function postJSON(url, body) {
    return fetch(url, {
      method: 'POST',
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify(body || {})
    }).then(function (r) {
      return r.json().catch(function () { return {}; }).then(function (j) {
        return { ok: r.ok, status: r.status, body: j };
      });
    });
  }
  function esc(s) {
    return String(s).replace(/[&<>"]/g, function (c) {
      return { '&': '&amp;', '<': '&lt;', '>': '&gt;', '"': '&quot;' }[c];
    });
  }
  function show(el) { el.hidden = false; }
  function relTime(iso) {
    if (!iso) return '';
    var t = new Date(iso).getTime();
    if (isNaN(t)) return '';
    var m = Math.round((Date.now() - t) / 60000);
    if (m < 1) return 'now';
    if (m < 60) return m + 'm';
    var h = Math.round(m / 60);
    if (h < 24) return h + 'h';
    return Math.round(h / 24) + 'd';
  }
  // one plain-text line out of a hub message (strip markdown noise)
  function preview(s) {
    return String(s || '').split('\n')[0]
      .replace(/\*\*|__|~~|`/g, '').trim().slice(0, 90);
  }

  // ── money lock: server-gated proxy + session PIN ───────────────────────────
  // The gate is the server (/hub/cuentas/* requires X-Money-Pin); this is just
  // presentation. PIN lives in sessionStorage only — masked again per tab.
  var PIN_KEY = 'caguastore.moneyPin';
  var moneyCard = document.getElementById('w-money');
  var pinVeil = document.getElementById('pin-veil');
  var pinPop = document.getElementById('pin-pop');
  var pinIn = document.getElementById('pin-in');

  function unlocked() { return !!sessionStorage.getItem(PIN_KEY); }
  function applyLockUI() { document.body.classList.toggle('unlocked', unlocked()); }

  function fmtMoney(cents) {
    var n = Math.round(cents / 100);
    return '$' + n.toLocaleString('en-US');
  }

  function loadMoney(pin) {
    return fetch('/hub/cuentas/summary', { headers: { 'X-Money-Pin': pin } })
      .then(function (r) {
        if (r.status === 401) { sessionStorage.removeItem(PIN_KEY); applyLockUI(); throw new Error('401'); }
        if (!r.ok) throw new Error('' + r.status);
        return r.json();
      })
      .then(function (s) {
        var m = (s.this_month && s.this_month[0]) || null;
        document.getElementById('w-money-v').textContent =
          m ? fmtMoney(m.net_cents) + ' net' : '—';
        var rc = (s.receivables && s.receivables.count) || 0;
        var rTotal = 0;
        var totals = (s.receivables && s.receivables.totals) || {};
        Object.keys(totals).forEach(function (k) { rTotal += totals[k]; });
        document.getElementById('w-money-s').textContent =
          rc ? rc + ' receivable · ' + fmtMoney(rTotal) : 'net este mes · 0 receivable';
        moneyCard.classList.remove('wg-locked');
      });
  }

  // mercado widget — the portfolio's tickers, live day change + trading state.
  // Holdings (which tickers, how much) come from the PIN-gated proxy; quotes
  // (public market data) from /api/quotes (Yahoo, 5-min server cache).
  function fmtK(usd) {
    return usd >= 1000 ? '$' + (usd / 1000).toFixed(1) + 'k' : '$' + Math.round(usd);
  }
  var pfPositions = null;   // sorted by value desc, quotable only
  var pfTotal = 0;
  var pfQuotes = {};        // yahoo symbol -> quote
  var quoteTimer = null;

  function yahooSym(t) {
    if (!t || t === 'USDT') return null;
    if (t === 'BTC' || t === 'ETH' || t === 'SOL') return t + '-USD';
    return t;
  }

  function renderMarket() {
    if (!pfPositions) return;
    var anyOpen = false;
    var rows = pfPositions.map(function (p) {
      var q = pfQuotes[p._sym];
      var chg = q && q.changePct != null ? q.changePct : null;
      var open = !!(q && q.trading);
      if (open && !(q && q.crypto)) anyOpen = true;
      return '<span class="mkt-row">' +
        '<span class="mkt-dot' + (open ? ' on' : '') + '"></span>' +
        '<b class="mkt-tkr">' + esc(p.ticker) + '</b>' +
        '<span class="mkt-hold">' + esc(fmtK(p.value || 0)) + '</span>' +
        '<span class="mkt-price">' + (q ? esc(q.price >= 100 ? q.price.toFixed(0) : q.price.toFixed(2)) : '—') + '</span>' +
        '<span class="mkt-chg' + (chg == null ? '' : chg >= 0 ? ' up' : ' dn') + '">' +
        (chg == null ? '' : (chg >= 0 ? '+' : '−') + Math.abs(chg).toFixed(2) + '%') + '</span></span>';
    });
    document.getElementById('w-market-list').innerHTML = rows.join('');
    document.getElementById('w-market-s').textContent =
      fmtK(pfTotal) + ' total · bolsa ' + (anyOpen ? 'abierta' : 'cerrada') + ' · cripto 24/7';
    // home mini chip: total + value-weighted day move
    var wSum = 0, wTot = 0;
    pfPositions.forEach(function (p) {
      var q = pfQuotes[p._sym];
      if (q && q.changePct != null) { wSum += q.changePct * (p.value || 0); wTot += p.value || 0; }
    });
    document.getElementById('w-mkt-mini-v').textContent = fmtK(pfTotal);
    document.getElementById('w-mkt-mini-s').textContent = wTot
      ? 'mercado ' + (wSum >= 0 ? '+' : '−') + Math.abs(wSum / wTot).toFixed(2) + '% hoy'
      : 'mercado';
    document.getElementById('w-mkt-mini').classList.remove('wg-locked');
  }

  function loadQuotes() {
    if (!pfPositions || !pfPositions.length) return;
    var syms = pfPositions.map(function (p) { return p._sym; }).join(',');
    getJSON('/api/quotes?symbols=' + encodeURIComponent(syms)).then(function (d) {
      (d.quotes || []).forEach(function (q) { pfQuotes[q.symbol] = q; });
      renderMarket();
    }).catch(function () {});
  }

  function loadPortfolio(pin) {
    return fetch('/hub/portfolio/data', { headers: { 'X-Money-Pin': pin } })
      .then(function (r) {
        if (!r.ok) throw new Error('' + r.status);
        return r.json();
      })
      .then(function (d) {
        var pos = (d.positions || []).slice().sort(function (a, b) {
          return (b.value || 0) - (a.value || 0);
        });
        pfTotal = 0;
        pos.forEach(function (p) { pfTotal += p.value || 0; });
        pfPositions = pos.filter(function (p) {
          p._sym = yahooSym(p.ticker);
          return !!p._sym;
        }).slice(0, 8);
        renderMarket();
        document.getElementById('w-market').classList.remove('wg-locked');
        loadQuotes();
        applyMktMode();
      });
  }

  function openPin() {
    pinVeil.hidden = false;
    pinPop.hidden = false;
    pinIn.value = '';
    pinIn.focus();
  }
  function closePin() {
    pinVeil.hidden = true;
    pinPop.hidden = true;
  }
  pinVeil.addEventListener('click', closePin);
  pinPop.addEventListener('submit', function (e) {
    e.preventDefault();
    var pin = pinIn.value.trim();
    if (!pin) return;
    loadMoney(pin).then(function () {
      sessionStorage.setItem(PIN_KEY, pin);
      applyLockUI();
      closePin();
      loadPortfolio(pin).catch(function () {});
    }).catch(function () {
      pinPop.classList.remove('shake');
      void pinPop.offsetWidth; // restart animation
      pinPop.classList.add('shake');
      pinIn.value = '';
      pinIn.focus();
    });
  });

  // any locked widget: deliberate unlock step before navigation
  document.addEventListener('click', function (e) {
    var wg = e.target.closest ? e.target.closest('.wg-locked') : null;
    if (wg && !unlocked()) {
      e.preventDefault();
      openPin();
    }
  }, true);

  // Private tiles never navigate while locked — deliberate unlock step first.
  document.addEventListener('click', function (e) {
    var tile = e.target.closest ? e.target.closest('.tile.priv') : null;
    if (tile && !unlocked()) {
      e.preventDefault();
      openPin();
    }
  }, true);

  applyLockUI();
  if (unlocked()) {
    loadMoney(sessionStorage.getItem(PIN_KEY)).catch(function () {});
    loadPortfolio(sessionStorage.getItem(PIN_KEY)).catch(function () {});
  }

  // ── polybot chip: whole-account total + today's realized, nothing else ─────
  function loadPolybot() {
    return fetch('/hub/polybot/widget')
      .then(function (r) { return r.ok ? r.json() : null; })
      .then(function (w) {
        if (!w || typeof w.total !== 'number') return;
        var hoy = w.today_realized || 0;
        document.getElementById('w-polybot-v').textContent =
          '$' + w.total.toFixed(0);
        var s = document.getElementById('w-polybot-s');
        s.textContent = 'polybet · hoy ' + (hoy < 0 ? '-' : '+') +
          '$' + Math.abs(hoy).toFixed(2);
        s.style.color = hoy < 0 ? 'var(--bad, #f87171)' : '';
        document.getElementById('w-polybot').hidden = false;
      });
  }
  loadPolybot().catch(function () {});

  // ── toast ──────────────────────────────────────────────────────────────────
  var toastTimer = null;
  function toast(msg, ok) {
    var el = document.getElementById('toast');
    if (!el) {
      el = document.createElement('div');
      el.id = 'toast';
      document.body.appendChild(el);
    }
    el.className = 'toast' + (ok ? ' ok' : '');
    el.textContent = msg;
    el.hidden = false;
    clearTimeout(toastTimer);
    toastTimer = setTimeout(function () { el.hidden = true; }, 2600);
  }

  // ── pulse — consolidated, actionable notifications ─────────────────────────
  // hermes channels with unread (skip the quick-prompt scratch channel) plus
  // any catalog app whose LED reads down. Each item: ✓ resolve (mark read /
  // dismiss) or 🤖 dispatch an agent through hermes. Quiet when empty.
  var pulse = document.getElementById('pulse');
  var pulseList = document.getElementById('pulse-list');
  var dismissed = {};       // slug/name -> true, session-scoped
  try { dismissed = JSON.parse(sessionStorage.getItem('caguastore.dismissed') || '{}'); }
  catch (e) { dismissed = {}; }
  function dismiss(key) {
    dismissed[key] = true;
    try { sessionStorage.setItem('caguastore.dismissed', JSON.stringify(dismissed)); }
    catch (e) { /* private mode */ }
  }

  function pulseActions(kind, key) {
    return '<span class="pulse-acts">' +
      '<button type="button" class="pulse-act" data-act="ok" data-kind="' + esc(kind) +
      '" data-key="' + esc(key) + '" title="resolve" aria-label="mark resolved">' +
      '<svg viewBox="0 0 24 24"><use href="#i-check" xlink:href="#i-check"/></svg></button>' +
      '<button type="button" class="pulse-act" data-act="agent" data-kind="' + esc(kind) +
      '" data-key="' + esc(key) + '" title="send an agent" aria-label="send an agent">' +
      '<svg viewBox="0 0 24 24"><use href="#i-bot" xlink:href="#i-bot"/></svg></button></span>';
  }

  var lastChannels = [];
  function renderPulse(channels) {
    if (channels) lastChannels = channels;
    var items = [];
    (lastChannels || []).forEach(function (c) {
      if (!c.unread || c.name === 'quick' || dismissed['ch:' + c.name]) return;
      items.push('<div class="pulse-it" style="--h:275">' +
        '<a class="pulse-main" href="' + esc(HERMES_URL + '/#c/' + encodeURIComponent(c.name)) + '">' +
        '<span class="pulse-dot"></span>' +
        '<span class="pulse-body"><span class="pulse-name">' + esc(c.name) +
        '<span class="pulse-time">' + esc(relTime(c.last_ts)) + '</span>' +
        '<span class="pulse-n">' + esc(String(c.unread)) + '</span></span>' +
        '<span class="pulse-text">' + esc(preview(c.last_text)) + '</span></span></a>' +
        pulseActions('ch', c.name) + '</div>');
    });
    // apps down — read off the server-rendered tiles
    Array.prototype.forEach.call(document.querySelectorAll('.tile.down'), function (t) {
      var slug = t.dataset.slug || '';
      if (dismissed['app:' + slug]) return;
      items.push('<div class="pulse-it pulse-warn">' +
        '<a class="pulse-main" href="' + esc(t.href) + '">' +
        '<span class="pulse-dot"></span>' +
        '<span class="pulse-body"><span class="pulse-name">' +
        esc(t.querySelector('.label').textContent) + '</span>' +
        '<span class="pulse-text">app is down</span></span></a>' +
        pulseActions('app', slug) + '</div>');
    });
    pulseList.innerHTML = items.slice(0, 6).join('');
    document.getElementById('pulse-n').textContent = items.length ? String(items.length) : '';
    pulse.hidden = !items.length || !!q.value;
  }

  function channelByName(name) {
    return (lastChannels || []).filter(function (c) { return c.name === name; })[0];
  }

  pulseList.addEventListener('click', function (e) {
    var b = e.target.closest ? e.target.closest('.pulse-act') : null;
    if (!b) return;
    e.preventDefault();
    var kind = b.dataset.kind, key = b.dataset.key, act = b.dataset.act;
    if (act === 'ok') {
      if (kind === 'ch') {
        var c = channelByName(key);
        if (c && c.last_id) {
          postJSON('/hub/hermes/channels/' + encodeURIComponent(key) + '/read',
            { last_id: c.last_id }).catch(function () {});
        }
        if (c) c.unread = 0;
      }
      dismiss(kind + ':' + key);
      renderPulse(null);
      toast('resolved · ' + key, true);
      return;
    }
    // dispatch an agent through hermes
    b.disabled = true;
    var channel, text;
    if (kind === 'app') {
      channel = 'hermes';
      text = 'caguastore: la app "' + key + '" se ve caída (LED down en el launcher). ' +
        'Investiga en caguaserver y levántala; avísame qué era.';
    } else {
      channel = key;
      text = 'revisa lo pendiente de arriba y encárgate — resuélvelo tú; ' +
        'si necesitas algo mío, dímelo claro y corto.';
    }
    postJSON('/hub/hermes/send', { channel: channel, text: text }).then(function (r) {
      if (!r.ok) throw new Error('send');
      dismiss(kind + ':' + key);
      renderPulse(null);
      toast('agente avisado · ' + key, true);
    }).catch(function () {
      b.disabled = false;
      toast('no llegó a hermes', false);
    });
  });

  // ── correo / whatsapp widgets — parsed from the sweep bots' hub posts ──────
  // correo channel: "🔴" marks mails needing a reply (fallback: "📧 N nuevos");
  // pulso channel: "💬 N chat(s) esperan respuesta".
  function updateCounts(channels) {
    (channels || []).forEach(function (c) {
      var txt = String(c.last_text || '');
      if (c.name === 'correo') {
        var reds = (txt.match(/🔴/g) || []).length;
        var nuevo = txt.match(/📧\s*(\d+)/);
        var n = reds || (nuevo ? parseInt(nuevo[1], 10) : 0);
        document.getElementById('w-correo-v').textContent = String(n);
        document.getElementById('w-correo-s').textContent =
          'correo ' + (reds ? 'por responder' : 'nuevos') + ' · ' + relTime(c.last_ts);
        var wc = document.getElementById('w-correo');
        wc.href = hermesChat('correo');
        wc.classList.toggle('wg-zero', !n);
        show(wc);
      }
      if (c.name === 'pulso') {
        var m = txt.match(/(\d+)\s*chat/i);
        if (!m) return;
        var wn = parseInt(m[1], 10);
        document.getElementById('w-whats-v').textContent = String(wn);
        document.getElementById('w-whats-s').textContent =
          'whatsapp esperan · ' + relTime(c.last_ts);
        var ww = document.getElementById('w-whats');
        ww.href = hermesChat('pulso');
        ww.classList.toggle('wg-zero', !wn);
        show(ww);
      }
    });
  }

  function refreshPulse() {
    getJSON('/hub/hermes/channels').then(function (chs) {
      renderPulse(chs);
      updateCounts(chs);
    }).catch(function () { renderPulse(null); });
  }
  refreshPulse();
  setInterval(refreshPulse, 90000);

  // ── heart widget — live bpm + last-hour sparkline (vitals/WHOOP) ───────────
  function drawSpark(el, points) {
    if (!points || points.length < 2) { el.innerHTML = ''; return; }
    var min = Infinity, max = -Infinity;
    points.forEach(function (v) { if (v < min) min = v; if (v > max) max = v; });
    var span = (max - min) || 1;
    var step = 100 / (points.length - 1);
    var d = points.map(function (v, i) {
      return (i * step).toFixed(1) + ',' + (24 - ((v - min) / span) * 22).toFixed(1);
    }).join(' ');
    el.innerHTML = '<polyline points="' + d + '"/>';
  }
  function loadHeart() {
    getJSON('/hub/vitals/vitals').then(function (v) {
      var hr = v.hr || {};
      if (!hr.bpm) return;
      document.getElementById('w-heart-v').textContent = hr.bpm;
      var lh = hr.lastHour || {};
      var fresh = v.freshness || {};
      var age = fresh.ageSeconds != null && fresh.ageSeconds < 300 ? 'en vivo' :
        relTime(v.generatedAtIso);
      document.getElementById('w-heart-s').textContent =
        (lh.min ? lh.min + '–' + lh.max + ' última hora · ' : '') + age;
      // downsample the raw stream to ≤48 points for the sparkline
      var stream = hr.stream || [];
      if (stream.length > 1) {
        var stride = Math.max(1, Math.floor(stream.length / 48));
        var pts = [];
        for (var i = 0; i < stream.length; i += stride) pts.push(stream[i].bpm);
        drawSpark(document.getElementById('w-heart-spark'), pts);
      }
      show(document.getElementById('w-heart'));
    }).catch(function () {});
  }
  loadHeart();
  setInterval(loadHeart, 60000);

  // ── marcar widget — one-tap context marks into the vitals journal ──────────
  (function () {
    var status = document.getElementById('w-marks-status');
    var whenRow = document.getElementById('w-marks-when');
    var tagRow = document.getElementById('w-marks-tags');
    if (!status || !whenRow || !tagRow) return;
    var offset = 0, statusT = 0;
    whenRow.addEventListener('click', function (e) {
      var b = e.target.closest('.mkwh');
      if (!b) return;
      offset = parseInt(b.dataset.off, 10) || 0;
      whenRow.querySelectorAll('.mkwh').forEach(function (x) { x.classList.toggle('on', x === b); });
    });
    tagRow.addEventListener('click', function (e) {
      var b = e.target.closest('.mkch');
      if (!b || b.disabled) return;
      var tag = b.dataset.tag;
      var ts = Math.floor(Date.now() / 1000) - offset;
      b.disabled = true;
      fetch('/hub/vitals/journal', {
        method: 'POST',
        headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify({ tag: tag, ts: ts })
      }).then(function (r) { return r.json(); }).then(function (d) {
        if (!d.ok) throw 0;
        b.classList.add('sent');
        setTimeout(function () { b.classList.remove('sent'); }, 1600);
        var t = new Date(d.ts * 1000);
        status.textContent = b.textContent + ' · ' +
          ('0' + t.getHours()).slice(-2) + ':' + ('0' + t.getMinutes()).slice(-2) + ' ✓';
        // reset the time row to "ahora" so a stale offset can't mislabel the next mark
        offset = 0;
        whenRow.querySelectorAll('.mkwh').forEach(function (x) {
          x.classList.toggle('on', x.dataset.off === '0');
        });
      }).catch(function () {
        status.textContent = 'no se pudo guardar';
      }).finally(function () {
        b.disabled = false;
        clearTimeout(statusT);
        statusT = setTimeout(function () { status.textContent = ''; }, 6000);
      });
    });
  })();

  // ── peso widget — MXN vs world currencies (frankfurter/ECB, no key) ────────
  getJSON('https://api.frankfurter.dev/v1/latest?base=MXN&symbols=USD,EUR,GBP,JPY,BRL')
    .then(function (d) {
      var r = d.rates || {};
      function per(code) { return r[code] ? 1 / r[code] : null; }
      var usd = per('USD');
      if (!usd) return;
      document.getElementById('w-fx-t').innerHTML =
        [['USD', usd], ['EUR', per('EUR')], ['GBP', per('GBP')], ['BRL', per('BRL')]]
          .filter(function (x) { return x[1]; })
          .map(function (x) {
            return '<span class="fx-r"><b>' + x[0] + '</b>' + x[1].toFixed(2) + '</span>';
          }).join('');
      show(document.getElementById('w-fx'));
    }).catch(function () {});

  // ── SPA router — screens + bottom tabs ─────────────────────────────────────
  var SCREENS = { inicio: 'scr-home', mercado: 'scr-market', noticias: 'scr-news', apps: 'scr-apps' };
  var curScreen = 'inicio';
  function showScreen(name) {
    if (!SCREENS[name]) name = 'inicio';
    curScreen = name;
    Object.keys(SCREENS).forEach(function (k) {
      document.getElementById(SCREENS[k]).hidden = k !== name;
    });
    Array.prototype.forEach.call(document.querySelectorAll('.tab[data-scr]'), function (t) {
      t.classList.toggle('on', t.getAttribute('href') === '#' + name);
    });
    if (name === 'mercado' && mktMode() === 'manual') loadQuotes();
    if (name === 'noticias') loadNews();
    document.getElementById('screens').scrollTop = 0;
  }
  function route() { showScreen((location.hash || '#inicio').slice(1)); }
  window.addEventListener('hashchange', route);

  // ── folds — collapse anything to its essence, persisted ────────────────────
  var FOLD_KEY = 'caguastore.folds';
  var folds = {};
  try { folds = JSON.parse(localStorage.getItem(FOLD_KEY) || '{}'); } catch (e) { folds = {}; }
  function applyFolds() {
    Array.prototype.forEach.call(document.querySelectorAll('[data-fold]'), function (el) {
      el.classList.toggle('folded', !!folds[el.dataset.fold]);
    });
  }
  document.addEventListener('click', function (e) {
    var b = e.target.closest ? e.target.closest('[data-fold-btn]') : null;
    if (!b) return;
    e.preventDefault();
    e.stopPropagation();
    var k = b.dataset.foldBtn;
    folds[k] = !folds[k];
    try { localStorage.setItem(FOLD_KEY, JSON.stringify(folds)); } catch (err) {}
    applyFolds();
  }, true);
  applyFolds();

  // ── mercado refresh modes ──────────────────────────────────────────────────
  var MKT_MODE_KEY = 'caguastore.mktMode';
  function mktMode() { return localStorage.getItem(MKT_MODE_KEY) || '5min'; }
  function applyMktMode() {
    var m = mktMode();
    Array.prototype.forEach.call(document.querySelectorAll('#mkt-seg .seg-b'), function (b) {
      b.classList.toggle('on', b.dataset.mode === m);
    });
    clearInterval(quoteTimer);
    if (m === 'live') quoteTimer = setInterval(loadQuotes, 60000);
    else if (m === '5min') quoteTimer = setInterval(loadQuotes, 300000);
  }
  document.getElementById('mkt-seg').addEventListener('click', function (e) {
    var b = e.target.closest ? e.target.closest('.seg-b') : null;
    if (!b) return;
    localStorage.setItem(MKT_MODE_KEY, b.dataset.mode);
    applyMktMode();
    loadQuotes();
  });
  document.getElementById('mkt-refresh').addEventListener('click', function (e) {
    e.preventDefault();
    e.stopPropagation();
    loadQuotes();
    toast('cotizaciones al día', true);
  });
  applyMktMode();

  // ── noticias — tunable feeds (google MX default + custom RSS) ──────────────
  var FEEDS_KEY = 'caguastore.feeds';
  function getFeeds() {
    try { return JSON.parse(localStorage.getItem(FEEDS_KEY) || '[]'); } catch (e) { return []; }
  }
  function setFeeds(f) {
    try { localStorage.setItem(FEEDS_KEY, JSON.stringify(f)); } catch (e) {}
  }
  function renderFeedChips() {
    var custom = getFeeds();
    var html = '<span class="chip on">méxico</span>' + custom.map(function (f, i) {
      return '<span class="chip on">' + esc(f.name) +
        '<button type="button" class="feed-x" data-fi="' + i + '" aria-label="quitar">&times;</button></span>';
    }).join('');
    document.getElementById('feeds').innerHTML = html;
  }
  document.getElementById('feeds').addEventListener('click', function (e) {
    var b = e.target.closest ? e.target.closest('.feed-x') : null;
    if (!b) return;
    var f = getFeeds();
    f.splice(parseInt(b.dataset.fi, 10), 1);
    setFeeds(f);
    renderFeedChips();
    loadNews(true);
  });
  document.getElementById('feed-add').addEventListener('submit', function (e) {
    e.preventDefault();
    var name = document.getElementById('feed-name').value.trim() || 'feed';
    var url = document.getElementById('feed-url').value.trim();
    if (!/^https:\/\//.test(url)) { toast('la url debe ser https', false); return; }
    var f = getFeeds();
    f.push({ name: name, url: url });
    setFeeds(f);
    document.getElementById('feed-name').value = '';
    document.getElementById('feed-url').value = '';
    renderFeedChips();
    loadNews(true);
  });

  var newsLoaded = false;
  function loadNews(force) {
    if (newsLoaded && !force) return;
    newsLoaded = true;
    renderFeedChips();
    var jobs = [getJSON('/api/news').then(function (d) {
      return (d.items || []).map(function (n) {
        return { title: n.title, src: n.source || 'méxico', ts: n.ts, link: n.link };
      });
    }).catch(function () { return []; })];
    getFeeds().forEach(function (f) {
      jobs.push(getJSON('/api/rss?url=' + encodeURIComponent(f.url)).then(function (d) {
        return (d.items || []).map(function (n) {
          return { title: n.title, src: f.name, ts: n.ts, link: n.link };
        });
      }).catch(function () { return []; }));
    });
    Promise.all(jobs).then(function (lists) {
      var all = [];
      lists.forEach(function (l) { all = all.concat(l); });
      all.sort(function (a, b) {
        return (Date.parse(b.ts) || 0) - (Date.parse(a.ts) || 0);
      });
      document.getElementById('w-news-list').innerHTML = all.slice(0, 30).map(function (n) {
        return '<a class="news-it" href="' + esc(n.link || '#') + '" target="_blank" rel="noopener">' +
          '<span class="news-t">' + esc(n.title) + '</span>' +
          '<span class="news-src">' + esc(n.src) + '</span></a>';
      }).join('');
    });
  }

  // ── quick prompt (`>` mode) — any model, through hermes ────────────────────
  var MODEL_KEY = 'caguastore.model';
  var modelsRow = document.getElementById('models');
  var askPanel = document.getElementById('ask');
  var askQText = document.getElementById('ask-q-text');
  var askModel = document.getElementById('ask-model');
  var askA = document.getElementById('ask-a');
  var models = [];          // [{id,label,default}]
  var modelsLoaded = false;
  var askPollTimer = null;

  function pickedModel() {
    var saved = localStorage.getItem(MODEL_KEY);
    var hit = models.filter(function (m) { return m.id === saved; })[0];
    if (hit) return hit;
    return models.filter(function (m) { return m.default; })[0] || models[0] || null;
  }

  function renderModels() {
    var cur = pickedModel();
    modelsRow.innerHTML = models.map(function (m) {
      return '<button type="button" class="chip' +
        (cur && m.id === cur.id ? ' on' : '') + '" data-model="' + esc(m.id) + '">' +
        esc(m.label) + '</button>';
    }).join('');
  }
  modelsRow.addEventListener('click', function (e) {
    var b = e.target.closest ? e.target.closest('[data-model]') : null;
    if (!b) return;
    localStorage.setItem(MODEL_KEY, b.dataset.model);
    renderModels();
    q.focus();
  });

  function loadModels() {
    if (modelsLoaded) return Promise.resolve();
    return getJSON('/hub/hermes/models').then(function (d) {
      models = (d && d.models) || [];
      modelsLoaded = true;
      renderModels();
    }).catch(function () { renderModels(); });
  }

  function stopAskPoll() {
    if (askPollTimer) { clearTimeout(askPollTimer); askPollTimer = null; }
  }

  function closeAsk() {
    stopAskPoll();
    askPanel.hidden = true;
  }
  document.getElementById('ask-close').addEventListener('click', function () {
    closeAsk();
    q.focus();
  });

  function pollReply(afterId, deadline) {
    askPollTimer = setTimeout(function () {
      getJSON('/hub/hermes/messages?channel=quick&after_id=' + afterId).then(function (msgs) {
        var reply = (msgs || []).filter(function (m) { return m.sender !== 'user'; })[0];
        if (reply) {
          askA.textContent = reply.text || '(empty reply)';
          askA.classList.remove('thinking');
          // keep quick chatter out of the pulse feed
          postJSON('/hub/hermes/channels/quick/read', { last_id: reply.id }).catch(function () {});
          return;
        }
        if (Date.now() > deadline) {
          askA.textContent = 'no reply yet — it will land in hermes.';
          askA.classList.remove('thinking');
          return;
        }
        pollReply(afterId, deadline);
      }).catch(function () {
        if (Date.now() > deadline) {
          askA.textContent = 'lost the connection — check hermes.';
          askA.classList.remove('thinking');
        } else {
          pollReply(afterId, deadline);
        }
      });
    }, 1500);
  }

  function sendAsk(text) {
    var m = pickedModel();
    stopAskPoll();
    askPanel.hidden = false;
    askQText.textContent = text;
    askModel.textContent = m ? m.label : 'hermes';
    askA.textContent = 'thinking';
    askA.classList.add('thinking');
    // ensure the scratch channel exists (409 = already there), pin the model,
    // then send. Failures surface in the panel instead of dying silently.
    postJSON('/hub/hermes/channels', { name: 'quick' }).then(function () {
      return m ? postJSON('/hub/hermes/channels/quick/model', { model: m.id }) : null;
    }).then(function () {
      return postJSON('/hub/hermes/send', { channel: 'quick', text: text });
    }).then(function (r) {
      if (!r || !r.ok || !r.body || !r.body.id) throw new Error('send failed');
      pollReply(r.body.id, Date.now() + 120000);
    }).catch(function () {
      askA.textContent = 'could not reach hermes.';
      askA.classList.remove('thinking');
    });
  }


  // ── search / command bar ───────────────────────────────────────────────────
  var q = document.getElementById('q');
  var qClear = document.getElementById('q-clear');
  var cmd = document.getElementById('cmd');
  var nowStrip = document.getElementById('widgets');
  var taskHits = document.getElementById('task-hits');
  var hitList = document.getElementById('hit-list');
  var noHits = document.getElementById('no-hits');
  var tiles = Array.prototype.slice.call(document.querySelectorAll('.tile'));
  var cats = Array.prototype.slice.call(document.querySelectorAll('.cat:not(.task-hits)'));
  var sel = -1; // index into visible tiles
  var anyTileHit = true;

  tiles.forEach(function (t) {
    t._name = t.querySelector('.label').textContent.toLowerCase();
    t._hay = (t._name + ' ' + (t.dataset.slug || '') + ' ' + (t.dataset.tag || '') + ' ' +
      (t.dataset.cat || '')).toLowerCase();
  });

  function askMode() { return q.value.charAt(0) === '>'; }

  // subsequence match; returns match positions in `name` when they land there
  function subseq(hay, needle) {
    var i = 0;
    for (var j = 0; j < hay.length && i < needle.length; j++) {
      if (hay[j] === needle[i]) i++;
    }
    return i === needle.length;
  }

  function highlight(el, needle) {
    var label = el.querySelector('.label');
    var name = label.textContent;
    if (!needle) { label.textContent = name; return; }
    var lower = name.toLowerCase();
    var out = '', i = 0;
    for (var j = 0; j < name.length; j++) {
      if (i < needle.length && lower[j] === needle[i]) {
        out += '<mark>' + esc(name[j]) + '</mark>';
        i++;
      } else {
        out += esc(name[j]);
      }
    }
    label.innerHTML = out;
  }

  function visibleTiles() {
    return tiles.filter(function (t) { return !t.classList.contains('q-hide'); });
  }

  function setSel(idx) {
    var vis = visibleTiles();
    tiles.forEach(function (t) { t.classList.remove('sel'); });
    if (!vis.length) { sel = -1; return; }
    sel = ((idx % vis.length) + vis.length) % vis.length;
    vis[sel].classList.add('sel');
    vis[sel].scrollIntoView({ block: 'nearest' });
  }

  function applyFilter() {
    var ask = askMode();
    cmd.classList.toggle('ask-on', ask);
    modelsRow.hidden = !ask;
    if (ask) loadModels();
    var needle = ask ? '' : q.value.trim().toLowerCase();
    qClear.hidden = !q.value;
    nowStrip.classList.toggle('q-hide', !!q.value);
    pulse.classList.toggle('q-hide', !!q.value);
    var any = false;
    tiles.forEach(function (t) {
      var hit = !needle || t._hay.indexOf(needle) !== -1 || subseq(t._hay, needle);
      t.classList.toggle('q-hide', !hit || ask);
      highlight(t, hit && needle && subseq(t._name, needle) ? needle : '');
      if (hit) any = true;
    });
    anyTileHit = any;
    cats.forEach(function (c) {
      var alive = c.querySelector('.tile:not(.q-hide)');
      c.classList.toggle('q-hide', !alive);
    });
    setSel(needle ? 0 : -1);
    if (!needle) { tiles.forEach(function (t) { t.classList.remove('sel'); }); sel = -1; }
    searchTasks(ask ? '' : needle);
    noHits.hidden = ask || any || !needle || !taskHits.hidden;
  }

  // task search — one lazy fetch of the full task list, filtered client-side
  var tasksPromise = null;
  var taskTimer = null;
  function searchTasks(needle) {
    if (!needle || needle.length < 2) {
      taskHits.hidden = true;
      hitList.innerHTML = '';
      return;
    }
    clearTimeout(taskTimer);
    taskTimer = setTimeout(function () {
      if (!tasksPromise) tasksPromise = getJSON('/hub/cc/tasks?project_id=all');
      tasksPromise.then(function (tasks) {
        if (q.value.trim().toLowerCase() !== needle) return; // stale
        var hits = tasks.filter(function (t) {
          var hay = (t.title + ' ' + (t.project_name || '')).toLowerCase();
          return hay.indexOf(needle) !== -1;
        }).slice(0, 8);
        hitList.innerHTML = hits.map(function (t) {
          return '<a class="hit" href="/board?project=' + t.project_id + '">' +
            '<span class="chip-status s-' + esc(t.status) + '">' +
            esc(t.status.replace('_', ' ')) + '</span>' +
            '<span class="hit-title">' + esc(t.title.replace(/\*\*/g, '')) + '</span>' +
            '<span class="hit-proj">' + esc(t.project_name || '') + '</span></a>';
        }).join('');
        taskHits.hidden = !hits.length;
        noHits.hidden = !!(visibleTiles().length || hits.length);
      }).catch(function () {
        tasksPromise = null; // retry on next keystroke
        taskHits.hidden = true;
      });
    }, 180);
  }

  q.addEventListener('input', function () {
    applyFilter();
    if (q.value && !askMode() && curScreen !== 'apps') location.hash = '#apps';
  });
  qClear.addEventListener('click', function () {
    q.value = '';
    applyFilter();
    q.focus();
  });

  document.addEventListener('keydown', function (e) {
    var ae = document.activeElement;
    var typing = ae === q;
    var otherField = ae && ae !== q &&
      (ae.tagName === 'INPUT' || ae.tagName === 'SELECT' || ae.tagName === 'TEXTAREA');
    if (otherField) {
      if (e.key === 'Escape' && !pinPop.hidden) closePin();
      return;
    }
    if (e.key === '/' && !typing) {
      e.preventDefault();
      q.focus();
      return;
    }
    if (!typing && (e.key.length === 1 && /[a-z0-9>]/i.test(e.key)) &&
        !e.metaKey && !e.ctrlKey && !e.altKey) {
      q.focus(); // plain typing focuses search; the char lands in the input
      return;
    }
    if (!typing) return;
    if (e.key === 'Escape') {
      q.value = '';
      applyFilter();
      closeAsk();
      q.blur();
    } else if (e.key === 'ArrowRight' || e.key === 'ArrowDown') {
      e.preventDefault();
      setSel(sel + 1);
    } else if (e.key === 'ArrowLeft' || e.key === 'ArrowUp') {
      e.preventDefault();
      setSel(sel - 1);
    } else if (e.key === 'Enter') {
      if (askMode()) {
        var prompt = q.value.slice(1).trim();
        if (prompt) sendAsk(prompt);
        return;
      }
      if (!q.value.trim()) return; // empty search: Enter is a no-op
      var vis = visibleTiles();
      var pick = vis[sel >= 0 ? sel : 0];
      if (pick) { window.location.href = pick.href; return; }
      var hit = hitList.querySelector('.hit');
      if (hit) { window.location.href = hit.href; return; }
      // nothing matched — fall through to hermes with the raw query
      var raw = q.value.trim();
      if (raw.length > 1 && !anyTileHit) sendAsk(raw);
    }
  });

  route();
})();
