<script setup lang="ts">
// The one banner surface: claw down, auth required, stream drops. An
// inline band, dismissible when the caller allows it.
defineProps<{
  variant: 'error' | 'warn' | 'info'
  dismissible?: boolean
}>()

defineEmits<{ dismiss: [] }>()
</script>

<template>
  <div class="banner" :class="variant" role="status">
    <span class="banner-mark" aria-hidden="true">{{
      variant === 'error' ? '✗' : variant === 'warn' ? '△' : '·'
    }}</span>
    <span class="banner-text"><slot /></span>
    <slot name="actions" />
    <button v-if="dismissible" class="banner-dismiss" title="dismiss" @click="$emit('dismiss')">
      ×
    </button>
  </div>
</template>

<style scoped>
.banner-mark {
  color: inherit;
  flex: none;
}

.banner.error .banner-mark {
  color: var(--error);
}

.banner.warn .banner-mark {
  color: var(--warn);
}

.banner.info .banner-mark {
  color: var(--info);
}

.banner-dismiss {
  padding: 0 8px;
  border: none;
  background: none;
  color: var(--text3);
  font-size: 14px;
}
</style>
