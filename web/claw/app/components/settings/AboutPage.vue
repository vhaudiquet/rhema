<script setup lang="ts">
// The About page: what this client and the daemon behind it are, and
// which public routes the client speaks. Static facts — the claw's own
// version is not on the wire, so none is claimed.
defineProps<{
  /** null while /healthz has not answered yet. */
  clawUp: boolean | null
}>()

// The routes this client actually calls; the full surface lives in
// crates/rhema-claw/src/http.rs.
const routes = [
  { method: 'GET', path: '/healthz', note: 'liveness, open without a token' },
  { method: 'GET', path: '/v1/sessions', note: 'the dispatch-key inventory' },
  { method: 'POST', path: '/v1/sessions/{key}/messages', note: 'message ingest, 202 + stream' },
  { method: 'GET', path: '/v1/sessions/{key}/events', note: 'the turn-event stream (SSE)' },
  { method: 'GET', path: '/v1/providers', note: 'constructible backends + active pair' },
  { method: 'PUT', path: '/v1/providers/active', note: 'switch the backend, persisted' },
  { method: 'PUT', path: '/v1/providers/{kind}/credentials', note: 'add a backend or replace its login' },
  { method: 'DELETE', path: '/v1/providers/{kind}', note: 'drop a backend from the roster' },
  { method: 'GET', path: '/v1/providers/{kind}/models', note: "a kind's live model listing" },
]
</script>

<template>
  <div class="page">
    <PageHeader title="About" subtitle="Identity, the engine behind it, and the surface this client speaks" />

    <div class="scroll">
      <div class="identity">
        <div class="avatar" aria-hidden="true">R</div>
        <div>
          <h2>Rhema</h2>
          <p>ῥῆμα, "the spoken word": words that do things.</p>
        </div>
      </div>

      <AppCallout variant="info" title="This is the claw's web client">
        A mirror over one running <code>rhema-claw</code> daemon: the
        conversation lives in the claw's sessions, the model choice in its
        settings store. A reload starts empty by design.
      </AppCallout>

      <SettingCard title="Stack" meta="read-only">
        <SettingRow title="Daemon" description="The resident agent serving this UI.">
          <span class="mono">rhema-claw</span>
        </SettingRow>
        <SettingRow title="Engine" description="The step engine every turn runs through.">
          <span class="mono">rhema-core</span>
        </SettingRow>
        <SettingRow title="Web client" description="This dashboard: Nuxt 4, one static bundle.">
          <span class="mono">rhema-claw-web 0.1.0</span>
        </SettingRow>
        <SettingRow title="Claw status" description="What /healthz last answered.">
          <AppBadge :variant="clawUp === null ? undefined : clawUp ? 'ok' : 'error'">
            {{ clawUp === null ? 'checking' : clawUp ? 'up' : 'down' }}
          </AppBadge>
        </SettingRow>
      </SettingCard>

      <SettingCard title="API surface" meta="the routes this client calls">
        <div v-for="route in routes" :key="`${route.method} ${route.path}`" class="route">
          <span class="method mono">{{ route.method }}</span>
          <span class="path mono">{{ route.path }}</span>
          <span class="note">{{ route.note }}</span>
        </div>
      </SettingCard>
    </div>
  </div>
</template>

<style scoped>
.page {
  display: flex;
  flex-direction: column;
  min-height: 0;
  min-width: 0;
  flex: 1;
}

.scroll {
  flex: 1;
  overflow-y: auto;
  padding: 0 24px 24px;
  display: flex;
  flex-direction: column;
  gap: 16px;
  max-width: 860px;
}

.identity {
  display: flex;
  align-items: center;
  gap: 16px;
  padding: 4px 2px;
}

.avatar {
  display: grid;
  place-items: center;
  width: 56px;
  height: 56px;
  border-radius: 14px;
  background: var(--accent);
  color: var(--on-accent);
  font-size: 26px;
  font-weight: 700;
  flex: none;
}

.identity h2 {
  margin: 0;
  font-size: 22px;
}

.identity p {
  margin: 2px 0 0;
  color: var(--text3);
}

.route {
  display: flex;
  align-items: baseline;
  gap: 12px;
  padding: 9px 16px;
}

.method {
  flex: none;
  width: 44px;
  color: var(--accent-text);
  font-size: 12px;
}

.path {
  color: var(--text);
  font-size: 12.5px;
  overflow: hidden;
  text-overflow: ellipsis;
  white-space: nowrap;
}

.note {
  flex: 1;
  text-align: right;
  color: var(--text3);
  font-size: 12px;
  min-width: 0;
}
</style>
