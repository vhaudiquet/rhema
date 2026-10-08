// The web claw is a client-side dashboard over an authenticated API:
// no server rendering, no secrets on the wire beyond the bearer token
// the user supplies. In dev, the claw-proxy server middleware (dev-only)
// keeps /v1 and /healthz same-origin against a locally running claw, so
// no CORS is involved; in the built static bundle that middleware is
// inert and the ingress owns the routing (see README.md).
export default defineNuxtConfig({
  compatibilityDate: '2025-07-15',
  ssr: false,
  devtools: { enabled: false },
  runtimeConfig: {
    // Dev-only: where the proxy middleware forwards the claw's routes.
    clawUpstream: 'http://127.0.0.1:7799',
    public: {
      // Base URL of the claw API; empty = same origin (dev proxy or
      // ingress). Cross-origin bases need CORS the claw does not send.
      clawBase: '',
      // Default bearer token; NUXT_PUBLIC_CLAW_TOKEN. The UI can
      // override it at runtime (stored in localStorage).
      clawToken: '',
    },
  },
  css: ['~/assets/css/main.css'],
  typescript: {
    strict: true,
  },
})
