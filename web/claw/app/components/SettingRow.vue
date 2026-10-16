<script setup lang="ts">
// One row inside a SettingCard: name and explanation on the left, the
// control (or value) on the right. Controls align center; tall custom
// content can opt out with `top`.
defineProps<{
  title: string
  description?: string
  /** Align the control to the row's top instead of its center. */
  top?: boolean
}>()
</script>

<template>
  <div class="row" :class="{ top }">
    <div class="text">
      <span class="name">{{ title }}</span>
      <span v-if="description" class="description">{{ description }}</span>
    </div>
    <div class="control">
      <slot />
    </div>
  </div>
</template>

<style scoped>
.row {
  display: flex;
  align-items: center;
  gap: 24px;
  padding: 13px 16px;
}

.row.top {
  align-items: flex-start;
}

.row.top .control {
  margin-top: 2px;
}

.text {
  flex: 1;
  min-width: 0;
  display: flex;
  flex-direction: column;
  gap: 1px;
}

.name {
  color: var(--text);
  font-weight: 600;
}

.description {
  color: var(--text3);
  font-size: 12.5px;
  overflow-wrap: anywhere;
}

.control {
  flex: none;
  max-width: 55%;
  min-width: 0;
  display: flex;
  align-items: center;
  gap: 8px;
}

.control :deep(select),
.control :deep(input) {
  width: 100%;
  min-width: 220px;
}

@media (max-width: 760px) {
  .row {
    flex-direction: column;
    align-items: stretch;
    gap: 8px;
  }

  .control,
  .control :deep(select),
  .control :deep(input) {
    max-width: none;
    width: 100%;
    min-width: 0;
  }
}
</style>
