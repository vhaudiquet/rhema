<script setup lang="ts">
// The Agent · General page: the mock's identity editor. The text
// fields save as a whole (empty clears, absent is untouched claw-side);
// the images apply the moment a file is chosen — uploading is not the
// kind of edit that needs a staging area. Presentational: data flows
// in as props, intent flows up as events.
import type { AgentIdentity, AgentImageKind, IdentityTexts } from '~/composables/useClaw'

const props = defineProps<{
  identity: AgentIdentity | null
  avatarUrl: string | null
  logoUrl: string | null
  /** An image upload or clear is in flight. */
  mutating: boolean
  saving: boolean
}>()

const emit = defineEmits<{
  save: [texts: IdentityTexts]
  uploadImage: [kind: AgentImageKind, file: File]
  clearImage: [kind: AgentImageKind]
}>()

// The draft: '' means "clear this field", matching the wire.
const draft = ref<IdentityTexts>({
  name: '',
  mark: '',
  initials: '',
  tagline: '',
  signoff: '',
})

const dirty = computed(() => {
  const identity = props.identity
  if (!identity) return false
  return (
    draft.value.name !== (identity.name ?? '') ||
    draft.value.mark !== (identity.mark ?? '') ||
    draft.value.initials !== (identity.initials ?? '') ||
    draft.value.tagline !== (identity.tagline ?? '') ||
    draft.value.signoff !== (identity.signoff ?? '')
  )
})

function discard(): void {
  const identity = props.identity
  if (!identity) return
  draft.value = {
    name: identity.name ?? '',
    mark: identity.mark ?? '',
    initials: identity.initials ?? '',
    tagline: identity.tagline ?? '',
    signoff: identity.signoff ?? '',
  }
}

// The draft starts empty, which reads as "dirty" against a saved
// identity — the first arrival syncs unconditionally, later ones only
// when the operator has not typed.
const initialized = ref(false)

watch(
  () => props.identity,
  (identity) => {
    if (!identity || (initialized.value && dirty.value)) return
    initialized.value = true
    draft.value = {
      name: identity.name ?? '',
      mark: identity.mark ?? '',
      initials: identity.initials ?? '',
      tagline: identity.tagline ?? '',
      signoff: identity.signoff ?? '',
    }
  },
  { immediate: true },
)

function save(): void {
  if (!dirty.value || props.saving) return
  emit('save', { ...draft.value })
}

// --- the images -------------------------------------------------------
const avatarInput = ref<HTMLInputElement | null>(null)
const logoInput = ref<HTMLInputElement | null>(null)

function pickImage(kind: AgentImageKind): void {
  ;(kind === 'avatar' ? avatarInput : logoInput).value?.click()
}

function onImageChosen(kind: AgentImageKind, event: Event): void {
  const input = event.target as HTMLInputElement
  const file = input.files?.[0]
  if (file) emit('uploadImage', kind, file)
  input.value = ''
}

const CAPS: Record<keyof IdentityTexts, number> = {
  name: 64,
  mark: 8,
  initials: 4,
  tagline: 120,
  signoff: 120,
}
</script>

<template>
  <div class="page">
    <PageHeader
      title="General"
      subtitle="The agent's identity — how every surface names and shows it"
    >
      <template #actions>
        <button :disabled="!dirty || saving" @click="discard">Discard</button>
        <button class="primary" :disabled="!dirty || saving" @click="save">
          {{ saving ? 'Saving…' : 'Save' }}
        </button>
      </template>
    </PageHeader>

    <div class="scroll">
      <div class="identity">
        <AgentAvatar
          :avatar-url="avatarUrl"
          :mark="draft.mark"
          :initials="draft.initials || draft.name.slice(0, 1)"
          :size="56"
          variant="initials"
        />
        <div class="identity-text">
          <h2>{{ draft.name || 'rhema' }}</h2>
          <p>{{ draft.tagline || 'the spoken word' }}</p>
        </div>
      </div>

      <AppCallout variant="info" title="This is the agent's identity, not the engine's">
        Everything in the interface uses the name, mark, and images here —
        the rail, the chat, the window title, the notifications. The
        engine's own name appears only where it is a component.
      </AppCallout>

      <SettingCard v-if="identity" title="Agent identity">
        <SettingRow
          title="Display name"
          description="Shown as the author of messages, in the window title, and in notifications."
        >
          <input
            v-model="draft.name"
            type="text"
            :maxlength="CAPS.name"
            placeholder="rhema"
            autocomplete="off"
            :disabled="saving"
          />
        </SettingRow>
        <SettingRow
          title="Mark"
          description="The glyph or emoji in the rail and the empty states. Defaults to ῥ."
        >
          <input
            v-model="draft.mark"
            type="text"
            :maxlength="CAPS.mark"
            placeholder="ῥ"
            autocomplete="off"
            :disabled="saving"
          />
        </SettingRow>
        <SettingRow
          title="Initials"
          description="Used when there is no mark and no avatar."
        >
          <input
            v-model="draft.initials"
            type="text"
            :maxlength="CAPS.initials"
            :placeholder="draft.name.slice(0, 1) || 'R'"
            autocomplete="off"
            :disabled="saving"
          />
        </SettingRow>
        <SettingRow title="Tagline" description="One line under the name on the agent pane.">
          <input
            v-model="draft.tagline"
            type="text"
            :maxlength="CAPS.tagline"
            placeholder="the spoken word"
            autocomplete="off"
            :disabled="saving"
          />
        </SettingRow>
        <SettingRow
          title="Sign-off"
          description="Appended to messages the agent sends outside the interface."
        >
          <input
            v-model="draft.signoff"
            type="text"
            :maxlength="CAPS.signoff"
            placeholder="optional"
            autocomplete="off"
            :disabled="saving"
          />
        </SettingRow>
      </SettingCard>

      <SettingCard
        v-if="identity"
        title="Avatar & logo"
        meta="png or jpeg, up to 2 MiB — uploads apply immediately"
      >
        <SettingRow
          title="Avatar"
          description="The square face in the chat and the rail."
        >
          <div class="image-control">
            <AgentAvatar :avatar-url="avatarUrl" :mark="identity.mark" :initials="identity.initials ?? identity.name?.slice(0, 1)" :size="40" variant="initials" />
            <button :disabled="mutating" @click="pickImage('avatar')">Upload…</button>
            <button v-if="identity.avatar" class="ghost" :disabled="mutating" @click="emit('clearImage', 'avatar')">
              Clear
            </button>
            <input
              ref="avatarInput"
              type="file"
              accept="image/png,image/jpeg,image/webp"
              @change="onImageChosen('avatar', $event)"
            />
          </div>
        </SettingRow>
        <SettingRow
          title="Logo"
          description="The mark the empty states and headers render."
        >
          <div class="image-control">
            <AgentAvatar :avatar-url="logoUrl" :mark="identity.mark" :size="40" variant="mark" />
            <button :disabled="mutating" @click="pickImage('logo')">Upload…</button>
            <button v-if="identity.logo" class="ghost" :disabled="mutating" @click="emit('clearImage', 'logo')">
              Clear
            </button>
            <input
              ref="logoInput"
              type="file"
              accept="image/png,image/jpeg,image/webp"
              @change="onImageChosen('logo', $event)"
            />
          </div>
        </SettingRow>
      </SettingCard>
    </div>
  </div>
</template>

<style scoped>
.page {
  display: flex;
  flex-direction: column;
  min-height: 0;
  min-width: 0;
  flex: 1;
}

.scroll {
  flex: 1;
  overflow-y: auto;
  padding: 0 24px 24px;
  display: flex;
  flex-direction: column;
  gap: 16px;
  max-width: 860px;
}

.identity {
  display: flex;
  align-items: center;
  gap: 16px;
  padding: 4px 2px;
}

.identity-text h2 {
  margin: 0;
  font-size: 22px;
}

.identity-text p {
  margin: 2px 0 0;
  color: var(--text3);
}

.image-control {
  display: flex;
  align-items: center;
  gap: 8px;
}

.image-control input[type='file'] {
  display: none;
}
</style>
