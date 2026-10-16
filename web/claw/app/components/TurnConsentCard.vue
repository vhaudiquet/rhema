<script setup lang="ts">
// The agent asking its human something: a secret to store, permission
// to act. Answering is intent only — the shell POSTs, and the card
// flips on the local echo or the stream's consent_resolved.
import { ref } from 'vue'
import type { ConsentCard } from '~/composables/useTranscript'
import type { ConsentAnswer } from '~/composables/useClaw'

const props = defineProps<{ consent: ConsentCard }>()

const emit = defineEmits<{
  answer: [id: string, answer: ConsentAnswer]
  deny: [id: string]
}>()

const draft = ref('')

function tone(consent: ConsentCard): 'pending' | 'ok' | 'denied' {
  if (consent.granted === null) return 'pending'
  return consent.granted ? 'ok' : 'denied'
}

function answerSecret(): void {
  const secret = draft.value.trim()
  if (secret === '') return
  emit('answer', props.consent.id, { secret })
  draft.value = ''
}
</script>

<template>
  <div class="consent-card" :class="tone(consent)">
    <div class="consent-head">
      <span class="badge">{{ consent.kind }}</span>
      <strong class="consent-summary">{{ consent.summary }}</strong>
      <span v-if="consent.granted !== null" class="consent-state mono">
        {{ consent.granted ? 'granted' : 'denied' }}
      </span>
    </div>
    <p v-if="consent.detail" class="consent-detail">{{ consent.detail }}</p>
    <div v-if="consent.granted === null" class="consent-actions">
      <template v-if="consent.kind === 'secret'">
        <input
          v-model="draft"
          type="password"
          placeholder="the secret, never shown again"
          autocomplete="off"
          @keydown.enter="answerSecret"
        />
        <button
          class="primary"
          :disabled="draft.trim() === ''"
          @click="answerSecret"
        >
          submit
        </button>
      </template>
      <template v-else>
        <button class="primary" @click="emit('answer', consent.id, { grant: true })">
          Approve
        </button>
        <button @click="emit('answer', consent.id, { grant: false })">
          Deny
        </button>
      </template>
      <button class="ghost" title="cancel without answering" @click="emit('deny', consent.id)">
        deny
      </button>
    </div>
  </div>
</template>

<style scoped>
.consent-card {
  display: flex;
  flex-direction: column;
  gap: 8px;
  padding: 10px 12px;
  background: var(--surface);
  border: 1px solid var(--border);
  border-left: 3px solid var(--warn);
  border-radius: 6px;
}

.consent-card.ok {
  border-left-color: var(--ok);
}

.consent-card.denied {
  border-left-color: var(--error);
}

.consent-head {
  display: flex;
  align-items: center;
  gap: 8px;
}

.consent-summary {
  flex: 1;
  min-width: 0;
  font-size: 13px;
}

.denied .consent-summary {
  color: var(--text3);
}

.consent-state {
  flex: none;
  font-size: 11px;
  letter-spacing: 0.08em;
  text-transform: uppercase;
  color: var(--ok);
}

.denied .consent-state {
  color: var(--error);
}

.consent-detail {
  margin: 0;
  color: var(--text2);
  font-size: 12.5px;
  overflow-wrap: anywhere;
}

.consent-actions {
  display: flex;
  align-items: center;
  flex-wrap: wrap;
  gap: 8px;
}

.consent-actions input {
  flex: 1;
  min-width: 160px;
}
</style>
