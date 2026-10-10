<script setup lang="ts">
// The one modal: a centered panel over a dimmed backdrop. Escape and a
// backdrop click close it; the panel stops the click from beneath. The
// caller owns open state and what the actions do.
import { onUnmounted, watch } from 'vue'

const props = defineProps<{
  open: boolean
  title: string
}>()

const emit = defineEmits<{ close: [] }>()

function onKeydown(event: KeyboardEvent): void {
  if (event.key === 'Escape' && props.open) emit('close')
}

watch(
  () => props.open,
  (open) => {
    if (!import.meta.client) return
    if (open) window.addEventListener('keydown', onKeydown)
    else window.removeEventListener('keydown', onKeydown)
  },
  { immediate: true },
)

onUnmounted(() => {
  if (import.meta.client) window.removeEventListener('keydown', onKeydown)
})
</script>

<template>
  <Teleport to="body">
    <div v-if="open" class="overlay" @click.self="emit('close')">
      <div class="panel" role="dialog" aria-modal="true" :aria-label="title">
        <header class="head">
          <strong class="title">{{ title }}</strong>
          <button class="ghost dismiss" title="close" @click="emit('close')">×</button>
        </header>
        <div class="body">
          <slot />
        </div>
        <footer v-if="$slots.actions" class="actions">
          <slot name="actions" />
        </footer>
      </div>
    </div>
  </Teleport>
</template>

<style scoped>
.overlay {
  position: fixed;
  inset: 0;
  z-index: 50;
  display: grid;
  place-items: center;
  padding: 24px;
  background: color-mix(in srgb, var(--sunken) 70%, transparent);
}

.panel {
  width: 100%;
  max-width: 520px;
  max-height: calc(100vh - 48px);
  display: flex;
  flex-direction: column;
  background: var(--surface);
  border: 1px solid var(--border-strong);
  border-radius: 10px;
  box-shadow: 0 18px 48px color-mix(in srgb, var(--sunken) 80%, transparent);
}

.head {
  display: flex;
  align-items: center;
  justify-content: space-between;
  gap: 12px;
  padding: 14px 16px;
  border-bottom: 1px solid var(--border);
}

.title {
  font-size: 15px;
}

.dismiss {
  padding: 0 6px;
  font-size: 15px;
}

.body {
  padding: 16px;
  overflow-y: auto;
}

.actions {
  display: flex;
  justify-content: flex-end;
  gap: 8px;
  padding: 12px 16px;
  border-top: 1px solid var(--border);
}
</style>
