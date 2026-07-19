/* caguastore board — kanban face over the Command Center (via /hub/cc/*).
   The CC stays the source of truth: every mutation is an API call, the DOM is
   optimistic with rollback. Dependency-free (repo rule: no npm, no CDN). */
(function () {
  'use strict';

  var API = '/hub/cc';
  var STATUSES = ['backlog', 'in_progress', 'done'];
  var DONE_LIMIT = 30;
  var PRIO_RANK = { critical: 0, urgent: 0, high: 1, normal: 2, low: 3 };

  var state = {
    tasks: [],
    projects: [],
    project: 'all', // 'all' or numeric id
    q: ''
  };

  var els = {
    board: document.getElementById('board'),
    chips: document.getElementById('chips'),
    status: document.getElementById('board-status'),
    sub: document.getElementById('board-sub'),
    q: document.getElementById('bq'),
    qClear: document.getElementById('bq-clear'),
    veil: document.getElementById('sheet-veil'),
    sheet: document.getElementById('add-sheet'),
    sheetCol: document.getElementById('sheet-col'),
    addTitle: document.getElementById('add-title'),
    addProject: document.getElementById('add-project'),
    toast: document.getElementById('toast')
  };

  // ── utils ──────────────────────────────────────────────────────────────────
  function getJSON(url, opts) {
    return fetch(url, opts).then(function (r) {
      if (!r.ok) {
        return r.text().then(function (t) {
          throw new Error(r.status + ' ' + t.slice(0, 120));
        });
      }
      return r.json();
    });
  }
  function post(url, body) {
    return getJSON(url, {
      method: 'POST',
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify(body)
    });
  }
  function esc(s) {
    return String(s).replace(/[&<>"]/g, function (c) {
      return { '&': '&amp;', '<': '&lt;', '>': '&gt;', '"': '&quot;' }[c];
    });
  }
  function cleanTitle(t) { return t.replace(/\*\*/g, ''); }

  var toastTimer = null;
  function toast(msg, ok) {
    els.toast.textContent = msg;
    els.toast.className = 'toast' + (ok ? ' ok' : '');
    els.toast.hidden = false;
    clearTimeout(toastTimer);
    toastTimer = setTimeout(function () { els.toast.hidden = true; }, 3200);
  }

  // ── rendering ──────────────────────────────────────────────────────────────
  function matchesFilters(t) {
    if (state.project !== 'all' && t.project_id !== state.project) return false;
    if (state.q) {
      var hay = (t.title + ' ' + (t.project_name || '')).toLowerCase();
      if (hay.indexOf(state.q) === -1) return false;
    }
    return true;
  }

  function sortTasks(a, b) {
    var pa = PRIO_RANK[a.priority] !== undefined ? PRIO_RANK[a.priority] : 2;
    var pb = PRIO_RANK[b.priority] !== undefined ? PRIO_RANK[b.priority] : 2;
    if (pa !== pb) return pa - pb;
    return (b.updated_at || '').localeCompare(a.updated_at || '');
  }

  function cardHTML(t) {
    var prio = t.priority && t.priority !== 'normal' ? ' prio-' + esc(t.priority) : '';
    return '<article class="card s-' + esc(t.status) + prio + '" data-id="' + t.id + '">' +
      '<div class="card-title">' + esc(cleanTitle(t.title)) + '</div>' +
      '<div class="card-meta">' +
      '<span class="card-grip" title="drag">&#8801;</span>' +
      '<span class="card-proj">' + esc(t.project_name || ('#' + t.project_id)) + '</span>' +
      (t.category ? '<span class="card-cat">' + esc(t.category) + '</span>' : '') +
      '<span class="card-tools">' +
      '<button class="t-prev" title="move left" aria-label="move left">&#8249;</button>' +
      '<button class="t-next" title="move right" aria-label="move right">&#8250;</button>' +
      '<button class="t-del" title="delete" aria-label="delete">&times;</button>' +
      '</span></div></article>';
  }

  function render() {
    STATUSES.forEach(function (st) {
      var col = document.getElementById('col-' + st);
      var list = state.tasks.filter(function (t) {
        return t.status === st && matchesFilters(t);
      }).sort(sortTasks);
      if (st === 'done') {
        list.sort(function (a, b) {
          return (b.done_date || b.updated_at || '').localeCompare(a.done_date || a.updated_at || '');
        });
        list = list.slice(0, DONE_LIMIT);
      }
      document.getElementById('n-' + st).textContent = list.length || '';
      col.innerHTML = list.length
        ? list.map(cardHTML).join('')
        : '<div class="col-empty">empty</div>';
    });
    els.board.setAttribute('aria-busy', 'false');
  }

  function renderChips() {
    var open = {};
    state.tasks.forEach(function (t) {
      if (t.status !== 'done') open[t.project_id] = (open[t.project_id] || 0) + 1;
    });
    var projs = state.projects.filter(function (p) { return open[p.id]; });
    projs.sort(function (a, b) {
      var sa = a.score || 0, sb = b.score || 0;
      if (sb !== sa) return sb - sa;
      return (open[b.id] || 0) - (open[a.id] || 0);
    });
    var html = '<button class="chip' + (state.project === 'all' ? ' on' : '') +
      '" data-project="all">all</button>';
    projs.forEach(function (p) {
      html += '<button class="chip' + (state.project === p.id ? ' on' : '') +
        '" data-project="' + p.id + '">' + esc(p.name) +
        '<span class="chip-n">' + open[p.id] + '</span></button>';
    });
    els.chips.innerHTML = html;
  }

  function refreshUI() {
    renderChips();
    render();
    var pname = state.project === 'all' ? 'all projects'
      : (state.projects.find(function (p) { return p.id === state.project; }) || {}).name;
    els.sub.textContent = 'command center · ' + (pname || state.project);
  }

  // ── mutations (optimistic, rollback on error) ──────────────────────────────
  function findTask(id) {
    return state.tasks.find(function (t) { return t.id === id; });
  }

  function setStatus(id, status) {
    var t = findTask(id);
    if (!t || t.status === status) return;
    var prev = t.status;
    t.status = status;
    if (status === 'done') t.done_date = new Date().toISOString().slice(0, 10);
    t.updated_at = new Date().toISOString();
    render();
    var el = document.querySelector('.card[data-id="' + id + '"]');
    if (el) el.classList.add('flash');
    post(API + '/tasks/' + id, { status: status }).catch(function (e) {
      t.status = prev;
      render();
      toast('move failed — reverted (' + e.message + ')');
    });
  }

  function cycle(id, dir) {
    var t = findTask(id);
    if (!t) return;
    var i = STATUSES.indexOf(t.status) + dir;
    if (i < 0 || i >= STATUSES.length) return;
    setStatus(id, STATUSES[i]);
  }

  function removeTask(id) {
    var t = findTask(id);
    if (!t) return;
    if (!window.confirm('Delete "' + cleanTitle(t.title).slice(0, 60) + '"? This is for tasks that will never happen.')) return;
    var idx = state.tasks.indexOf(t);
    state.tasks.splice(idx, 1);
    refreshUI();
    fetch(API + '/tasks/' + id, { method: 'DELETE' }).then(function (r) {
      if (!r.ok) throw new Error('HTTP ' + r.status);
      toast('deleted', true);
    }).catch(function (e) {
      state.tasks.splice(idx, 0, t);
      refreshUI();
      toast('delete failed — restored (' + e.message + ')');
    });
  }

  // ── quick add ──────────────────────────────────────────────────────────────
  var addStatus = 'backlog';
  function openSheet(status) {
    addStatus = status;
    els.sheetCol.textContent = status.replace('_', ' ');
    els.addProject.innerHTML = state.projects
      .slice()
      .sort(function (a, b) { return a.name.localeCompare(b.name); })
      .map(function (p) {
        return '<option value="' + p.id + '"' +
          (state.project === p.id ? ' selected' : '') + '>' + esc(p.name) + '</option>';
      }).join('');
    els.veil.hidden = false;
    els.sheet.hidden = false;
    els.addTitle.value = '';
    els.addTitle.focus();
  }
  function closeSheet() {
    els.veil.hidden = true;
    els.sheet.hidden = true;
  }

  els.sheet.addEventListener('submit', function (e) {
    e.preventDefault();
    var title = els.addTitle.value.trim();
    var pid = parseInt(els.addProject.value, 10);
    if (!title || !pid) return;
    closeSheet();
    post(API + '/tasks', {
      project_id: pid,
      title: title,
      priority: 'normal',
      source: 'agent'
    }).then(function (created) {
      var patch = addStatus !== 'backlog'
        ? post(API + '/tasks/' + created.id, { status: addStatus })
        : Promise.resolve(created);
      return patch.then(function () {
        created.status = addStatus;
        created.project_name = created.project_name ||
          ((state.projects.find(function (p) { return p.id === pid; }) || {}).name);
        state.tasks.push(created);
        refreshUI();
        toast('added to ' + addStatus.replace('_', ' '), true);
      });
    }).catch(function (e2) {
      toast('add failed (' + e2.message + ')');
    });
  });
  document.getElementById('add-cancel').addEventListener('click', closeSheet);
  els.veil.addEventListener('click', closeSheet);
  document.addEventListener('keydown', function (e) {
    if (e.key === 'Escape' && !els.sheet.hidden) closeSheet();
  });

  // ── event delegation: card tools + chips + add buttons ─────────────────────
  els.board.addEventListener('click', function (e) {
    var btn = e.target.closest('button');
    var card = e.target.closest('.card');
    if (btn && btn.classList.contains('col-add')) {
      openSheet(btn.dataset.status);
      return;
    }
    if (!btn || !card) return;
    var id = parseInt(card.dataset.id, 10);
    if (btn.classList.contains('t-next')) cycle(id, 1);
    else if (btn.classList.contains('t-prev')) cycle(id, -1);
    else if (btn.classList.contains('t-del')) removeTask(id);
  });

  els.chips.addEventListener('click', function (e) {
    var chip = e.target.closest('.chip');
    if (!chip) return;
    var v = chip.dataset.project;
    state.project = v === 'all' ? 'all' : parseInt(v, 10);
    var url = new URL(window.location.href);
    if (state.project === 'all') url.searchParams.delete('project');
    else url.searchParams.set('project', state.project);
    window.history.replaceState(null, '', url);
    refreshUI();
  });

  // ── filter box ─────────────────────────────────────────────────────────────
  els.q.addEventListener('input', function () {
    state.q = els.q.value.trim().toLowerCase();
    els.qClear.hidden = !state.q;
    render();
  });
  els.qClear.addEventListener('click', function () {
    els.q.value = '';
    state.q = '';
    els.qClear.hidden = true;
    render();
    els.q.focus();
  });

  // ── drag and drop (pointer events → works with touch via the grip) ─────────
  var drag = null; // { id, ghost, fromCol }
  els.board.addEventListener('pointerdown', function (e) {
    var grip = e.target.closest('.card-grip');
    if (!grip) return;
    var card = grip.closest('.card');
    if (!card) return;
    e.preventDefault();
    var rect = card.getBoundingClientRect();
    var ghost = card.cloneNode(true);
    ghost.classList.add('ghost');
    ghost.style.width = rect.width + 'px';
    ghost.style.left = rect.left + 'px';
    ghost.style.top = rect.top + 'px';
    document.body.appendChild(ghost);
    card.classList.add('dragging');
    drag = {
      id: parseInt(card.dataset.id, 10),
      card: card,
      ghost: ghost,
      dx: e.clientX - rect.left,
      dy: e.clientY - rect.top
    };
    grip.setPointerCapture(e.pointerId);
  });

  document.addEventListener('pointermove', function (e) {
    if (!drag) return;
    drag.ghost.style.left = (e.clientX - drag.dx) + 'px';
    drag.ghost.style.top = (e.clientY - drag.dy) + 'px';
    var over = document.elementFromPoint(e.clientX, e.clientY);
    var col = over && over.closest ? over.closest('.col') : null;
    document.querySelectorAll('.col').forEach(function (c) {
      c.classList.toggle('drop-hint', c === col);
    });
  });

  function endDrag(e) {
    if (!drag) return;
    var over = document.elementFromPoint(e.clientX, e.clientY);
    var col = over && over.closest ? over.closest('.col') : null;
    var id = drag.id;
    drag.ghost.remove();
    drag.card.classList.remove('dragging');
    document.querySelectorAll('.col').forEach(function (c) { c.classList.remove('drop-hint'); });
    drag = null;
    if (col) setStatus(id, col.dataset.status);
  }
  document.addEventListener('pointerup', endDrag);
  document.addEventListener('pointercancel', function () {
    if (!drag) return;
    drag.ghost.remove();
    drag.card.classList.remove('dragging');
    document.querySelectorAll('.col').forEach(function (c) { c.classList.remove('drop-hint'); });
    drag = null;
  });

  // ── boot ───────────────────────────────────────────────────────────────────
  var params = new URLSearchParams(window.location.search);
  if (params.get('project')) state.project = parseInt(params.get('project'), 10) || 'all';

  Promise.all([
    getJSON(API + '/tasks?project_id=all'),
    getJSON(API + '/projects')
  ]).then(function (res) {
    state.tasks = res[0];
    state.projects = res[1];
    els.status.hidden = true;
    refreshUI();
  }).catch(function (e) {
    els.status.textContent = 'command center unreachable — ' + e.message;
  });

  if ('serviceWorker' in navigator) {
    navigator.serviceWorker.register('/sw.js').catch(function () {});
  }
})();
