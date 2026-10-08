// Dev-only same-origin proxy: forwards the claw's routes to a locally
// running daemon (`clawUpstream`, NUXT_CLAW_UPSTREAM to override) so
// the browser sees one origin and CORS never enters the picture.
//
// Why not `nitro.devProxy`: its httpxy pass never delivers responses to
// bodyless GETs (observed against nitro 2.13), which is exactly the
// shape of /healthz and /v1/sessions. `proxyRequest` is the same
// primitive nitro's route-rule proxy uses.
//
// The built bundle is static — with `import.meta.dev` false this falls
// through and the request 404s against the static output; in a real
// deployment the ingress owns /v1 and /healthz routing (README.md).
import { proxyRequest } from 'h3'

export default defineEventHandler((event) => {
  if (!import.meta.dev) return
  const { pathname } = getRequestURL(event)
  if (pathname !== '/healthz' && !pathname.startsWith('/v1/')) return
  return proxyRequest(event, useRuntimeConfig(event).clawUpstream + pathname)
})
