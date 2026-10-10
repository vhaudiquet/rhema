<script setup lang="ts">
// One small round state dot with an honest tone: ok, warn, error, or
// idle gray. The only place status color lives at a glance.
defineProps<{
  tone: 'ok' | 'warn' | 'error' | 'idle'
}>()
</script>

<template>
  <span class="dot" :class="tone" aria-hidden="true" />
</template>

<style scoped>
.dot {
  display: inline-block;
  flex: none;
  width: 8px;
  height: 8px;
  border-radius: 50%;
  background: var(--text3);
}

.dot.ok {
  background: var(--ok);
}

.dot.warn {
  background: var(--warn);
}

.dot.error {
  background: var(--error);
}
</style>
