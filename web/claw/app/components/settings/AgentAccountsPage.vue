<script setup lang="ts">
// The Agent · Accounts page: logins the agent itself holds on external
// services. Deliberately empty until the claw serves an accounts
// surface — the page exists so the menu is honest about the shape, not
// to fake a roster.
</script>

<template>
  <div class="page">
    <PageHeader
      title="Accounts"
      subtitle="Who the agent is on the outside"
    />
    <div class="scroll">
      <SettingCard title="Agent accounts" meta="none yet">
        <div class="empty">
          <p>
            No agent accounts yet. An account is a login the agent holds
            on an external service — a GitHub identity it answers issues
            as, an email it sends from — distinct from the model
            providers above (what it thinks with) and the forge watches
            (what it looks at).
          </p>
          <p class="dim">
            The surface for adding them arrives with the first service
            that needs one; the shape is settled: a named account with
            its service, the credential stored claw-side, and never
            rendered back.
          </p>
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

.empty p {
  margin: 0;
  padding: 14px 16px;
  color: var(--text2);
}

.empty .dim {
  color: var(--text3);
}
</style>
