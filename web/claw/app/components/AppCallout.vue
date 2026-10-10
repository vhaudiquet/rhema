<script setup lang="ts">
// The inline callout: a bordered note with a tone — the "restart
// required"/"changes not applied" voice from the designs. One per
// situation, inside the page flow, dismissible by the caller.
defineProps<{
  variant: 'info' | 'ok' | 'warn' | 'error'
  title?: string
  dismissible?: boolean
}>()

defineEmits<{ dismiss: [] }>()

const marks: Record<string, string> = {
  info: 'i',
  ok: '✓',
  warn: '↺',
  error: '✗',
}
</script>

<template>
  <div class="callout" :class="variant" role="status">
    <span class="mark" aria-hidden="true">{{ marks[variant] }}</span>
    <div class="body">
      <strong v-if="title" class="title">{{ title }}</strong>
      <div class="text"><slot /></div>
    </div>
    <button v-if="dismissible" class="ghost dismiss" title="dismiss" @click="$emit('dismiss')">
      ×
    </button>
  </div>
</template>

<style scoped>
.callout {
  display: flex;
  align-items: flex-start;
  gap: 10px;
  padding: 11px 14px;
  border: 1px solid var(--border);
  border-left: 3px solid var(--info);
  border-radius: 8px;
  background: color-mix(in srgb, var(--info) 7%, var(--surface));
}

.callout.ok {
  border-left-color: var(--ok);
  background: color-mix(in srgb, var(--ok) 7%, var(--surface));
}

.callout.warn {
  border-left-color: var(--warn);
  background: color-mix(in srgb, var(--warn) 7%, var(--surface));
}

.callout.error {
  border-left-color: var(--error);
  background: color-mix(in srgb, var(--error) 7%, var(--surface));
}

.mark {
  flex: none;
  width: 18px;
  text-align: center;
  font-weight: 700;
  color: var(--info);
}

.ok .mark {
  color: var(--ok);
}

.warn .mark {
  color: var(--warn);
}

.error .mark {
  color: var(--error);
}

.body {
  flex: 1;
  min-width: 0;
}

.title {
  display: block;
  color: var(--text);
}

.text {
  color: var(--text2);
  overflow-wrap: anywhere;
}

.dismiss {
  padding: 0 6px;
  font-size: 14px;
}
</style>
