<script setup lang="ts">
// The one page header: title, an optional subtitle line, and the page's
// actions (Save/Discard-style buttons) on the right.
defineProps<{
  title: string
  subtitle?: string
}>()
</script>

<template>
  <header class="page-head">
    <div class="text">
      <h1>{{ title }}</h1>
      <p v-if="subtitle">{{ subtitle }}</p>
    </div>
    <div class="actions">
      <slot name="actions" />
    </div>
  </header>
</template>

<style scoped>
.page-head {
  display: flex;
  align-items: flex-start;
  justify-content: space-between;
  gap: 16px;
  padding: 18px 24px 14px;
}

h1 {
  margin: 0;
  font-size: 19px;
  font-weight: 700;
}

.text p {
  margin: 2px 0 0;
  color: var(--text3);
}

.actions {
  display: flex;
  align-items: center;
  gap: 8px;
  flex: none;
}
</style>
