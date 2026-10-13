<script setup lang="ts">
// The icon rail: the leftmost strip every view shares — the brand mark,
// one button per view, and the claw's pulse at the bottom. The active
// view is highlighted; navigation intent flows up, the shell decides.
defineProps<{
  view: 'chat' | 'settings'
  /** null while /healthz has not answered yet. */
  clawUp: boolean | null
  identity?: { mark: string | null; initials: string | null; name: string | null } | null
  avatarUrl?: string | null
}>()

defineEmits<{ navigate: [view: 'chat' | 'settings'] }>()
</script>

<template>
  <nav class="rail">
    <AgentAvatar
      variant="mark"
      :avatar-url="avatarUrl"
      :mark="identity?.mark"
      :size="36"
      class="mark"
      title="rhema, the spoken word"
    />

    <button
      class="rail-btn"
      :class="{ active: view === 'chat' }"
      title="Chat"
      aria-label="Chat"
      @click="$emit('navigate', 'chat')"
    >
      <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.7" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true">
        <path d="M21 11.5a7.5 7.5 0 0 1-7.5 7.5H8l-4.5 3V11.5a7.5 7.5 0 0 1 7.5-7.5h2.5a7.5 7.5 0 0 1 7.5 7.5Z" />
      </svg>
    </button>

    <button
      class="rail-btn"
      :class="{ active: view === 'settings' }"
      title="Settings"
      aria-label="Settings"
      @click="$emit('navigate', 'settings')"
    >
      <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.7" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true">
        <circle cx="12" cy="12" r="3" />
        <path d="M19.4 15a1.65 1.65 0 0 0 .33 1.82l.06.06a2 2 0 0 1 0 2.83 2 2 0 0 1-2.83 0l-.06-.06a1.65 1.65 0 0 0-1.82-.33 1.65 1.65 0 0 0-1 1.51V21a2 2 0 0 1-2 2 2 2 0 0 1-2-2v-.09A1.65 1.65 0 0 0 9 19.4a1.65 1.65 0 0 0-1.82.33l-.06.06a2 2 0 0 1-2.83 0 2 2 0 0 1 0-2.83l.06-.06a1.65 1.65 0 0 0 .33-1.82 1.65 1.65 0 0 0-1.51-1H3a2 2 0 0 1-2-2 2 2 0 0 1 2-2h.09A1.65 1.65 0 0 0 4.6 9a1.65 1.65 0 0 0-.33-1.82l-.06-.06a2 2 0 0 1 0-2.83 2 2 0 0 1 2.83 0l.06.06a1.65 1.65 0 0 0 1.82.33H9a1.65 1.65 0 0 0 1-1.51V3a2 2 0 0 1 2-2 2 2 0 0 1 2 2v.09a1.65 1.65 0 0 0 1 1.51 1.65 1.65 0 0 0 1.82-.33l.06-.06a2 2 0 0 1 2.83 0 2 2 0 0 1 0 2.83l-.06.06a1.65 1.65 0 0 0-.33 1.82V9a1.65 1.65 0 0 0 1.51 1H21a2 2 0 0 1 2 2 2 2 0 0 1-2 2h-.09a1.65 1.65 0 0 0-1.51 1z" />
      </svg>
    </button>

    <div class="spacer" />

    <span
      class="pulse"
      :title="clawUp === null ? 'health unknown' : clawUp ? '/healthz ok' : '/healthz failing'"
    >
      <span class="dot" :class="{ up: clawUp === true, down: clawUp === false }" aria-hidden="true" />
    </span>
    <AgentAvatar
      :avatar-url="avatarUrl"
      :initials="identity?.initials ?? identity?.name?.slice(0, 1)"
      :size="30"
      class="avatar"
      title="rhema"
    />
  </nav>
</template>

<style scoped>
.rail {
  display: flex;
  flex-direction: column;
  align-items: center;
  gap: 6px;
  width: 56px;
  flex: none;
  padding: 12px 0;
  background: var(--sunken);
  border-right: 1px solid var(--border);
}

.mark {
  margin-bottom: 10px;
}

.rail-btn {
  display: grid;
  place-items: center;
  width: 36px;
  height: 36px;
  padding: 0;
  border-radius: 9px;
  border-color: transparent;
  background: none;
  color: var(--text3);
}

.rail-btn svg {
  width: 19px;
  height: 19px;
}

.rail-btn:hover:not(:disabled) {
  background: var(--elevated);
  color: var(--text);
}

.rail-btn.active {
  background: color-mix(in srgb, var(--accent) 16%, transparent);
  color: var(--accent-text);
}

.spacer {
  flex: 1;
}

.dot {
  display: block;
  width: 8px;
  height: 8px;
  border-radius: 50%;
  background: var(--text3);
}

.dot.up {
  background: var(--ok);
}

.dot.down {
  background: var(--error);
}

.avatar {
  margin-top: 8px;
}
</style>
