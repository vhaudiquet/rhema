<script setup lang="ts">
// A turn's stream-ordered middle: tool rows, pieces of the thinking
// trace, the agent's asks, its handovers — each at the position the
// stream delivered it, so an ask sits at the tool call that raised it
// and thinking sits between the calls it precedes.
import type { TurnEntry } from '~/composables/useTranscript'
import type { ConsentAnswer } from '~/composables/useClaw'

defineProps<{
  entries: TurnEntry[]
  /** A live turn's thinking blocks start expanded; finished ones collapse. */
  openThinking?: boolean
}>()

const emit = defineEmits<{
  consentAnswer: [id: string, answer: ConsentAnswer]
  consentDeny: [id: string]
  downloadAttachment: [id: string, name: string]
}>()
</script>

<template>
  <div v-if="entries.length > 0" class="entries">
    <template v-for="(entry, index) in entries" :key="index">
      <TurnToolRow v-if="entry.kind === 'tool'" :row="entry.row" />
      <details v-else-if="entry.kind === 'thinking'" class="thinking" :open="openThinking">
        <summary>thinking</summary>
        <pre>{{ entry.text }}</pre>
      </details>
      <TurnConsentCard
        v-else-if="entry.kind === 'consent'"
        :consent="entry.consent"
        @answer="(id, answer) => emit('consentAnswer', id, answer)"
        @deny="(id) => emit('consentDeny', id)"
      />
      <TurnAttachmentCard
        v-else
        :attachment="entry.attachment"
        @download="(id, name) => emit('downloadAttachment', id, name)"
      />
    </template>
  </div>
</template>

<style scoped>
.entries {
  display: flex;
  flex-direction: column;
  gap: 4px;
  margin-bottom: 10px;
}

.thinking {
  color: var(--text3);
}

.thinking summary {
  cursor: pointer;
  font-size: 11px;
  letter-spacing: 0.1em;
  text-transform: uppercase;
}

.thinking pre {
  margin: 6px 0 0;
  padding: 8px 10px;
  background: var(--surface);
  border: 1px solid var(--border);
  border-radius: 6px;
  color: var(--text3);
  font-style: italic;
  white-space: pre-wrap;
  overflow-wrap: anywhere;
}
</style>
