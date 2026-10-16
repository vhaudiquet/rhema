<script setup lang="ts">
// The agent handing a file over: a head row naming it, the image (when
// there is one) under it, capped so a screenshot cannot take the
// stream; code and file download on click.
import type { AttachmentCard } from '~/composables/useTranscript'

defineProps<{ attachment: AttachmentCard }>()

const emit = defineEmits<{
  download: [id: string, name: string]
}>()

const MEDIA_GLYPHS: Record<AttachmentCard['media'], string> = {
  image: '▣',
  code: '⌗',
  file: '▤',
}

/** B / KB / MB, one decimal past 1 KB — a card label, not precision. */
function formatBytes(size: number): string {
  if (size < 1024) return `${size} B`
  if (size < 1024 * 1024) return `${(size / 1024).toFixed(1)} KB`
  return `${(size / (1024 * 1024)).toFixed(1)} MB`
}
</script>

<template>
  <div class="attachment-card">
    <div class="attachment-head">
      <span
        class="attachment-glyph"
        aria-hidden="true"
        :title="`${attachment.media} attachment`"
      >{{ MEDIA_GLYPHS[attachment.media] }}</span>
      <span class="attachment-name mono">{{ attachment.name }}</span>
      <span class="attachment-size mono">{{ formatBytes(attachment.size) }}</span>
      <button
        v-if="attachment.media !== 'image'"
        class="ghost"
        :title="`download ${attachment.name}`"
        @click="emit('download', attachment.id, attachment.name)"
      >
        ↓ download
      </button>
    </div>
    <img
      v-if="attachment.media === 'image' && attachment.url"
      :src="attachment.url"
      :alt="attachment.caption ?? attachment.name"
      class="attachment-image"
    />
    <p v-if="attachment.caption" class="attachment-caption">
      {{ attachment.caption }}
    </p>
  </div>
</template>

<style scoped>
.attachment-card {
  display: flex;
  flex-direction: column;
  gap: 8px;
  max-width: 480px;
  padding: 10px 12px;
  background: var(--surface);
  border: 1px solid var(--border);
  border-radius: 6px;
}

.attachment-head {
  display: flex;
  align-items: center;
  gap: 8px;
}

.attachment-glyph {
  flex: none;
  color: var(--text3);
}

.attachment-name {
  flex: 1;
  min-width: 0;
  overflow: hidden;
  text-overflow: ellipsis;
  white-space: nowrap;
  color: var(--text);
}

.attachment-size {
  flex: none;
  color: var(--text3);
  font-size: 11px;
}

.attachment-image {
  max-width: 100%;
  max-height: 240px;
  align-self: flex-start;
  border: 1px solid var(--border);
  border-radius: 8px;
}

.attachment-caption {
  margin: 0;
  color: var(--text3);
  font-size: 12.5px;
  overflow-wrap: anywhere;
}
</style>
