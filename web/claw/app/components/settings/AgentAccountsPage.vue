<script setup lang="ts">
// The Agent · Accounts page: logins the agent itself holds on external
// services. Presentational like its siblings — data flows in as props,
// intent flows up as events; the secret field is write-only by the
// wire's shape, and the roster can only ever render what `GET /v1/
// accounts` serves, which is the redacted record.
import type { ClawAccount, NewAccount } from '~/composables/useClaw'

const props = defineProps<{
  accounts: ClawAccount[] | null
  loading: boolean
  /** A save or forget is in flight. */
  mutating: boolean
}>()

const emit = defineEmits<{
  add: [account: NewAccount]
  remove: [account: ClawAccount]
}>()

const adding = ref(false)

// The form's own shape: every field a string (empty = omit on the
// wire), so the template never fights `NewAccount`'s optional fields.
const draft = ref({
  label: '',
  service: '',
  url: '',
  username: '',
  secret: '',
  secret_kind: 'token' as 'token' | 'password',
  watch: false,
})

const formValid = computed(
  () =>
    draft.value.label.trim() !== '' &&
    draft.value.service.trim() !== '' &&
    draft.value.secret.trim() !== '',
)

function submit(): void {
  if (!formValid.value || props.mutating) return
  // Empty optionals are omitted, never sent as "" strings.
  const payload: NewAccount = {
    label: draft.value.label.trim(),
    service: draft.value.service.trim().toLowerCase(),
    secret: draft.value.secret,
  }
  if (draft.value.url.trim() !== '') payload.url = draft.value.url.trim()
  if (draft.value.username.trim() !== '') payload.username = draft.value.username.trim()
  if (draft.value.secret_kind === 'password') payload.secret_kind = 'password'
  if (draft.value.watch) payload.watch = true
  emit('add', payload)
  draft.value = {
    label: '',
    service: '',
    url: '',
    username: '',
    secret: '',
    secret_kind: 'token',
    watch: false,
  }
  adding.value = false
}

// A forget cascades the account's derived watches, so it asks twice.
const confirming = ref<string | null>(null)

function forget(account: ClawAccount): void {
  if (props.mutating) return
  if (confirming.value !== account.id) {
    confirming.value = account.id
    return
  }
  confirming.value = null
  emit('remove', account)
}

function meta(account: ClawAccount): string {
  const parts = [account.service]
  if (account.username) parts.push(account.username)
  if (account.url) parts.push(account.url)
  return parts.join(' · ')
}
</script>

<template>
  <div class="page">
    <PageHeader title="Accounts" subtitle="Who the agent is on the outside">
      <template #actions>
        <button class="primary" :disabled="mutating" @click="adding = !adding">
          {{ adding ? 'Close' : 'Add account…' }}
        </button>
      </template>
    </PageHeader>
    <div class="scroll">
      <SettingCard v-if="adding" title="New account" meta="the secret is stored, never shown again">
        <SettingRow title="Label" description="A name for the roster, e.g. “acme releases”.">
          <input
            v-model="draft.label"
            type="text"
            maxlength="128"
            placeholder="acme releases"
            autocomplete="off"
            :disabled="mutating"
          />
        </SettingRow>
        <SettingRow
          title="Service"
          description="Lowercase: github, gitlab, or gitea — or any name for a service the agent only stores."
        >
          <input
            v-model="draft.service"
            type="text"
            placeholder="github"
            autocomplete="off"
            :disabled="mutating"
          />
        </SettingRow>
        <SettingRow title="URL" description="Base URL, for self-hosted instances.">
          <input
            v-model="draft.url"
            type="text"
            placeholder="https://git.example.net"
            autocomplete="off"
            :disabled="mutating"
          />
        </SettingRow>
        <SettingRow title="Username" description="The login name on the service.">
          <input
            v-model="draft.username"
            type="text"
            placeholder="optional"
            autocomplete="off"
            :disabled="mutating"
          />
        </SettingRow>
        <SettingRow title="Secret" description="An API token — or a password, which the agent stores but can never use.">
          <input
            v-model="draft.secret"
            type="password"
            placeholder="paste the credential"
            autocomplete="off"
            :disabled="mutating"
          />
        </SettingRow>
        <SettingRow title="Kind" description="What the secret authenticates as.">
          <select v-model="draft.secret_kind" :disabled="mutating">
            <option value="token">token</option>
            <option value="password">password</option>
          </select>
        </SettingRow>
        <SettingRow
          title="Follow notifications"
          description="For a forge service: watch everything addressed to this account, triaged into agent work."
        >
          <input v-model="draft.watch" type="checkbox" :disabled="mutating" />
        </SettingRow>
        <div class="actions">
          <button :disabled="mutating" @click="adding = false">Cancel</button>
          <button class="primary" :disabled="!formValid || mutating" @click="submit">
            {{ mutating ? 'Saving…' : 'Save account' }}
          </button>
        </div>
      </SettingCard>

      <SettingCard
        title="Agent accounts"
        :meta="accounts?.length ? `${accounts.length}` : 'none yet'"
      >
        <p v-if="loading" class="dim">loading…</p>
        <p v-else-if="!accounts?.length" class="dim">
          No agent accounts yet. An account is a login the agent holds on
          an external service — a GitHub identity it answers issues as,
          an email it sends from — distinct from the model providers
          (what it thinks with) and the forge watches (what it looks at).
          For a forge identity, “follow notifications” turns the account
          into watches automatically: review requests, assignments, and
          feedback become agent work, mentions surface to you.
        </p>
        <SettingRow
          v-for="account in accounts"
          :key="account.id"
          :title="account.label"
          :description="meta(account)"
        >
          <div class="roster-control">
            <span class="kind">{{ account.secret_kind }}</span>
            <button
              class="ghost"
              :disabled="mutating"
              @click="forget(account)"
            >
              {{
                confirming === account.id
                  ? 'Forget — also drops its watches?'
                  : 'Forget…'
              }}
            </button>
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

.actions {
  display: flex;
  justify-content: flex-end;
  gap: 8px;
  padding: 8px 16px 14px;
}

.dim {
  margin: 0;
  padding: 14px 16px;
  color: var(--text2);
}

.roster-control {
  display: flex;
  align-items: center;
  gap: 8px;
}

.kind {
  font-size: 12px;
  color: var(--text3);
}
</style>
