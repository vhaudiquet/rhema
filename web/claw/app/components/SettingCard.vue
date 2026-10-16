<script setup lang="ts">
// One titled card of setting rows or static facts — the repeating
// surface of every settings page. The header may carry a meta note on
// the right ("4 unset — inherit the primary model"-style).
defineProps<{
  title: string
  meta?: string
}>()
</script>

<template>
  <section class="card">
    <header class="card-head">
      <span class="title">{{ title }}</span>
      <span v-if="meta || $slots.meta" class="meta"><slot name="meta">{{ meta }}</slot></span>
    </header>
    <div class="card-body">
      <slot />
    </div>
  </section>
</template>
