/* caguastore service worker — cache the launcher shell so the home screen
   opens instantly (and offline) on the phone. Bump VERSION on asset changes. */
var VERSION = 'caguastore-v1';
var SHELL = [
  '/',
  '/static/store.css',
  '/static/app.css',
  '/static/htmx.min.js',
  '/static/icons/favicon.svg',
  '/static/icons/icon-192.png',
  '/static/icons/icon-512.png',
  '/manifest.webmanifest'
];

self.addEventListener('install', function (e) {
  e.waitUntil(
    caches.open(VERSION).then(function (c) { return c.addAll(SHELL); })
      .then(function () { return self.skipWaiting(); })
  );
});

self.addEventListener('activate', function (e) {
  e.waitUntil(
    caches.keys().then(function (keys) {
      return Promise.all(keys.filter(function (k) { return k !== VERSION; })
        .map(function (k) { return caches.delete(k); }));
    }).then(function () { return self.clients.claim(); })
  );
});

self.addEventListener('fetch', function (e) {
  var url = new URL(e.request.url);
  if (url.origin !== location.origin || e.request.method !== 'GET') return;

  // Static assets: cache-first (they only change with a deploy + VERSION bump).
  if (url.pathname.indexOf('/static/') === 0) {
    e.respondWith(
      caches.match(e.request).then(function (hit) {
        return hit || fetch(e.request).then(function (resp) {
          var copy = resp.clone();
          caches.open(VERSION).then(function (c) { c.put(e.request, copy); });
          return resp;
        });
      })
    );
    return;
  }

  // Pages (incl. '/'): network-first so live status is fresh, cache fallback
  // so the launcher still opens with the server unreachable.
  e.respondWith(
    fetch(e.request).then(function (resp) {
      if (url.pathname === '/') {
        var copy = resp.clone();
        caches.open(VERSION).then(function (c) { c.put('/', copy); });
      }
      return resp;
    }).catch(function () {
      return caches.match(e.request).then(function (hit) {
        return hit || caches.match('/');
      });
    })
  );
});
