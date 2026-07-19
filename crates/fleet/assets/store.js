/* caguastore home — clock, SW, instant search (apps + CC tasks), now-strip.
   Dependency-free by design (repo rule: no npm, no CDN). */
(function () {
  'use strict';

  // ── clock ──────────────────────────────────────────────────────────────────
  var clock = document.getElementById('clock');
  var date = document.getElementById('date');
  var days = ['sun', 'mon', 'tue', 'wed', 'thu', 'fri', 'sat'];
  var months = ['jan', 'feb', 'mar', 'apr', 'may', 'jun', 'jul', 'aug', 'sep', 'oct', 'nov', 'dec'];
  function tick() {
    var d = new Date();
    clock.textContent =
      String(d.getHours()).padStart(2, '0') + ':' + String(d.getMinutes()).padStart(2, '0');
    date.textContent = days[d.getDay()] + ' ' + d.getDate() + ' ' + months[d.getMonth()];
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
  function esc(s) {
    return String(s).replace(/[&<>"]/g, function (c) {
      return { '&': '&amp;', '<': '&lt;', '>': '&gt;', '"': '&quot;' }[c];
    });
  }
  function show(el) { el.hidden = false; }

  // ── now strip ──────────────────────────────────────────────────────────────
  function fmtMoney(cents) {
    var n = Math.round(cents / 100);
    return '$' + n.toLocaleString('en-US');
  }

  // ── money lock: server-gated proxy + session PIN ───────────────────────────
  // The gate is the server (/hub/cuentas/* requires X-Money-Pin); this is just
  // presentation. PIN lives in sessionStorage only — masked again per tab.
  var PIN_KEY = 'caguastore.moneyPin';
  var moneyCard = document.getElementById('now-money');
  var pinVeil = document.getElementById('pin-veil');
  var pinPop = document.getElementById('pin-pop');
  var pinIn = document.getElementById('pin-in');

  function unlocked() { return !!sessionStorage.getItem(PIN_KEY); }
  function applyLockUI() { document.body.classList.toggle('unlocked', unlocked()); }

  function loadMoney(pin) {
    return fetch('/hub/cuentas/summary', { headers: { 'X-Money-Pin': pin } })
      .then(function (r) {
        if (r.status === 401) { sessionStorage.removeItem(PIN_KEY); applyLockUI(); throw new Error('401'); }
        if (!r.ok) throw new Error('' + r.status);
        return r.json();
      })
      .then(function (s) {
        var m = (s.this_month && s.this_month[0]) || null;
        document.getElementById('now-money-v').textContent =
          m ? fmtMoney(m.net_cents) + ' net' : '—';
        var rc = (s.receivables && s.receivables.count) || 0;
        var rTotal = 0;
        var totals = (s.receivables && s.receivables.totals) || {};
        Object.keys(totals).forEach(function (k) { rTotal += totals[k]; });
        document.getElementById('now-money-s').textContent =
          rc ? rc + ' receivable · ' + fmtMoney(rTotal) : 'this month · 0 receivable';
        moneyCard.classList.remove('now-locked');
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
    }).catch(function () {
      pinPop.classList.remove('shake');
      void pinPop.offsetWidth; // restart animation
      pinPop.classList.add('shake');
      pinIn.value = '';
      pinIn.focus();
    });
  });

  moneyCard.addEventListener('click', function (e) {
    if (unlocked()) return; // navigate to cuentas normally
    e.preventDefault();
    openPin();
  });

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
  }

  getJSON('/hub/hermes/channels').then(function (chs) {
    if (!Array.isArray(chs)) return;
    var unread = chs.reduce(function (a, c) { return a + (c.unread || 0); }, 0);
    document.getElementById('now-hermes-v').textContent =
      unread ? unread + ' unread' : 'inbox zero';
    document.getElementById('now-hermes-s').textContent = chs.length + ' channels';
    show(document.getElementById('now-hermes'));
  }).catch(function () {});

  getJSON('/hub/cc/next').then(function (projects) {
    if (!Array.isArray(projects)) return;
    var picks = projects.filter(function (p) { return !p.blocked && p.next_action; }).slice(0, 2);
    picks.forEach(function (p, i) {
      var card = document.getElementById('now-next-' + (i + 1));
      if (!card) return;
      card.href = '/board?project=' + p.project_id;
      document.getElementById('now-next-' + (i + 1) + '-v').textContent = p.next_action;
      document.getElementById('now-next-' + (i + 1) + '-s').textContent =
        p.name + ' · ' + (p.bucket || 'next');
      show(card);
    });
  }).catch(function () {});

  // ── search ─────────────────────────────────────────────────────────────────
  var q = document.getElementById('q');
  var qClear = document.getElementById('q-clear');
  var nowStrip = document.getElementById('now-strip');
  var taskHits = document.getElementById('task-hits');
  var hitList = document.getElementById('hit-list');
  var noHits = document.getElementById('no-hits');
  var tiles = Array.prototype.slice.call(document.querySelectorAll('.tile'));
  var cats = Array.prototype.slice.call(document.querySelectorAll('.cat:not(.task-hits)'));
  var sel = -1; // index into visible tiles

  tiles.forEach(function (t) {
    t._name = t.querySelector('.label').textContent.toLowerCase();
    t._hay = (t._name + ' ' + (t.dataset.slug || '') + ' ' + (t.dataset.tag || '') + ' ' +
      (t.dataset.cat || '')).toLowerCase();
  });

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
    var needle = q.value.trim().toLowerCase();
    qClear.hidden = !needle;
    nowStrip.classList.toggle('q-hide', !!needle);
    var any = false;
    tiles.forEach(function (t) {
      var hit = !needle || t._hay.indexOf(needle) !== -1 || subseq(t._hay, needle);
      t.classList.toggle('q-hide', !hit);
      highlight(t, hit && needle && subseq(t._name, needle) ? needle : '');
      if (hit) any = true;
    });
    cats.forEach(function (c) {
      var alive = c.querySelector('.tile:not(.q-hide)');
      c.classList.toggle('q-hide', !alive);
    });
    setSel(needle ? 0 : -1);
    if (!needle) { tiles.forEach(function (t) { t.classList.remove('sel'); }); sel = -1; }
    searchTasks(needle);
    noHits.hidden = any || !needle || !taskHits.hidden;
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

  q.addEventListener('input', applyFilter);
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
    if (!typing && e.key.length === 1 && /[a-z0-9]/i.test(e.key) &&
        !e.metaKey && !e.ctrlKey && !e.altKey) {
      q.focus(); // plain typing focuses search; the char lands in the input
      return;
    }
    if (!typing) return;
    if (e.key === 'Escape') {
      q.value = '';
      applyFilter();
      q.blur();
    } else if (e.key === 'ArrowRight' || e.key === 'ArrowDown') {
      e.preventDefault();
      setSel(sel + 1);
    } else if (e.key === 'ArrowLeft' || e.key === 'ArrowUp') {
      e.preventDefault();
      setSel(sel - 1);
    } else if (e.key === 'Enter') {
      var vis = visibleTiles();
      var pick = vis[sel >= 0 ? sel : 0];
      if (pick) window.location.href = pick.href;
      else {
        var hit = hitList.querySelector('.hit');
        if (hit) window.location.href = hit.href;
      }
    }
  });
})();
