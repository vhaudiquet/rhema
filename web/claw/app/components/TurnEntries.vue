<script setup lang="ts">
// A turn's stream-ordered middle: tool rows, the agent's asks, its
// handovers — each at the position the stream delivered it, so an ask
// sits at the tool call that raised it rather than at the turn's top.
import type { TurnEntry } from '~/composables/useTranscript'
import type { ConsentAnswer } from '~/composables/useClaw'

defineProps<{ entries: TurnEntry[] }>()

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
</style>
