// Cross-origin isolation for static hosts such as GitHub Pages. The initial
// navigation cannot be isolated because Pages does not let us set response
// headers. Once this worker controls the page, it adds COOP and COEP to every
// non-opaque response; index.html reloads after activation.
self.addEventListener("install", () => self.skipWaiting());
self.addEventListener("activate", (event) => event.waitUntil(self.clients.claim()));

self.addEventListener("fetch", (event) => {
  // Chrome rejects this cache mode unless the request is same-origin.
  if (event.request.cache === "only-if-cached" && event.request.mode !== "same-origin") {
    return;
  }

  event.respondWith((async () => {
    const response = await fetch(event.request);

    // An opaque cross-origin response has no readable headers. Leaving it
    // untouched lets COEP correctly reject it unless it has opted in via CORP.
    if (response.type === "opaque") return response;

    const headers = new Headers(response.headers);
    headers.set("Cross-Origin-Opener-Policy", "same-origin");
    headers.set("Cross-Origin-Embedder-Policy", "require-corp");
    // Fetch can expose a stream even for statuses whose responses are
    // required to have a null body. Passing that stream back to the Response
    // constructor rejects respondWith() (notably for conditional 304s).
    const body = [101, 103, 204, 205, 304].includes(response.status)
      ? null
      : response.body;
    return new Response(body, {
      status: response.status,
      statusText: response.statusText,
      headers,
    });
  })());
});
