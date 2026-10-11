<script setup lang="ts">
// The agent's face at one size. Two voices: `initials` (the chat, the
// sidebar, the rail's bottom — the avatar image when there is one, the
// initials when not) and `mark` (the rail's brand slot — the avatar
// image when there is one, the mark or emoji, ῥ by default). The
// precedence lives here and nowhere else.
const props = withDefaults(
  defineProps<{
    variant?: 'initials' | 'mark'
    /** The uploaded avatar's object URL, when the claw has one. */
    avatarUrl?: string | null
    /** The glyph or emoji; the mark voice defaults to ῥ. */
    mark?: string | null
    /** The initials voice's fallback, first letter of the name or R. */
    initials?: string | null
    size?: number
  }>(),
  {
    variant: 'initials',
    avatarUrl: null,
    mark: null,
    initials: null,
    size: 32,
  },
)

const glyph = computed(() =>
  props.variant === 'mark'
    ? props.mark?.trim() || 'ῥ'
    : props.initials?.trim() || 'R',
)
</script>

<template>
  <img
    v-if="avatarUrl"
    :src="avatarUrl"
    :style="{ width: `${size}px`, height: `${size}px` }"
    class="agent-avatar"
    alt=""
  />
  <span
    v-else
    class="agent-avatar agent-square"
    :style="{ width: `${size}px`, height: `${size}px`, fontSize: `${Math.round(size * 0.5)}px` }"
    aria-hidden="true"
  >{{ glyph }}</span>
</template>

<style scoped>
.agent-avatar {
  display: grid;
  place-items: center;
  flex: none;
  border-radius: 25%;
  object-fit: cover;
}

.agent-square {
  background: var(--accent);
  color: var(--on-accent);
  font-weight: 700;
  line-height: 1;
}
</style>
