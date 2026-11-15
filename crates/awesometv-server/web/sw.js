const CACHE = "awesometv-shell-v3";
const RUNTIME = "awesometv-runtime-v1";
const MEDIA = "awesometv-media-v1";
const MEDIA_MAX_ENTRIES = 60;
const SHELL = [
  "/",
  "/manifest.webmanifest",
  "/icons/icon-192.png",
  "/icons/icon-512.png",
  "/icons/icon-maskable-512.png",
  "/icons/apple-touch-icon.png",
];

self.addEventListener("install", (event) => {
  event.waitUntil(
    caches
      .open(CACHE)
      .then((cache) => cache.addAll(SHELL))
      .then(() => self.skipWaiting())
  );
});

self.addEventListener("activate", (event) => {
  event.waitUntil(
    caches
      .keys()
      .then((keys) =>
        Promise.all(
          keys
            .filter((key) => key !== CACHE && key !== RUNTIME && key !== MEDIA)
            .map((key) => caches.delete(key))
        )
      )
      .then(() => self.clients.claim())
  );
});

async function networkFirst(request, cacheName) {
  try {
    const response = await fetch(request);
    if (response.ok) {
      const cache = await caches.open(cacheName);
      cache.put(request, response.clone()).catch(() => {});
    }
    return response;
  } catch {
    const cached = await caches.match(request);
    if (cached) return cached;
    throw new Response("offline", { status: 503, statusText: "Offline" });
  }
}

// HLS segments are immutable once written, so a cached copy is always valid.
// This smooths retries, seeks, and brief network drops without risking stale
// live content. The cache is bounded by entry count.
async function cacheFirstMedia(request) {
  const cache = await caches.open(MEDIA);
  const cached = await cache.match(request);
  if (cached) return cached;
  const response = await fetch(request);
  if (response.ok) {
    cache.put(request, response.clone()).then(async () => {
      const keys = await cache.keys();
      for (let index = 0; index < keys.length - MEDIA_MAX_ENTRIES; index += 1) {
        await cache.delete(keys[index]);
      }
    });
  }
  return response;
}

self.addEventListener("fetch", (event) => {
  const request = event.request;
  if (request.method !== "GET") return;
  const url = new URL(request.url);
  if (url.origin !== self.location.origin) return;

  // Never cache the admin API or signed media routes.
  const isPublicApi = url.pathname.startsWith("/api/v1/public/");
  if (
    !isPublicApi &&
    (url.pathname.startsWith("/api/") ||
      url.pathname.startsWith("/cast/") ||
      url.pathname.startsWith("/catchup/") ||
      url.pathname.startsWith("/dvr/") ||
      url.pathname.startsWith("/cloud/"))
  ) {
    return;
  }

  if (isPublicApi) {
    event.respondWith(networkFirst(request, RUNTIME));
    return;
  }

  if (url.pathname.startsWith("/channel/") && url.pathname.endsWith(".ts")) {
    event.respondWith(cacheFirstMedia(request));
    return;
  }

  if (request.mode === "navigate") {
    event.respondWith(fetch(request).catch(() => caches.match("/")));
    return;
  }

  if (SHELL.includes(url.pathname) || url.pathname.startsWith("/icons/")) {
    event.respondWith(
      caches.match(request).then((cached) => cached || fetch(request))
    );
  }
});
