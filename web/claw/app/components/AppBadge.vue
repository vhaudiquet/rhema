<script setup lang="ts">
// The pill: one small status or tag word. Machine spellings (session
// keys, model ids) stay mono via the global `.badge` voice.
defineProps<{
  variant?: 'dim' | 'accent' | 'ok' | 'warn' | 'error'
}>()
</script>

<template>
  <span class="badge" :class="variant"><slot /></span>
</template>
