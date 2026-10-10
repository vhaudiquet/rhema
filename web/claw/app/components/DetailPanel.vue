<script setup lang="ts">
// The right-hand detail panel of a settings page: context about what
// the selected row means — credentials in the designs, backend facts
// here. Sticky, scrollable, never the primary flow.
defineProps<{
  title: string
}>()
</script>

<template>
  <aside class="detail">
    <header class="head">
      <span class="title">{{ title }}</span>
      <slot name="actions" />
    </header>
    <div class="body">
      <slot />
    </div>
  </aside>
</template>

<style scoped>
.detail {
  display: flex;
  flex-direction: column;
  min-height: 0;
  width: 320px;
  flex: none;
  border-left: 1px solid var(--border);
  background: var(--surface);
}

.head {
  display: flex;
  align-items: center;
  justify-content: space-between;
  gap: 8px;
  padding: 14px 16px;
  border-bottom: 1px solid var(--border);
}

.title {
  font-weight: 700;
  font-size: 14px;
}

.body {
  flex: 1;
  overflow-y: auto;
  padding: 16px;
  display: flex;
  flex-direction: column;
  gap: 14px;
}

@media (max-width: 1100px) {
  .detail {
    display: none;
  }
}
</style>
