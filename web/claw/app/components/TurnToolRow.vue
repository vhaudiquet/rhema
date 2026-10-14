<script setup lang="ts">
// One tool call in a turn: running, completed, or failed — shell lines
// for bash (the TUI's tool_row), glyph rows for everything else.
import type { ToolRow } from '~/composables/useTranscript'

defineProps<{ row: ToolRow }>()

/** Command rows render as shell lines, not glyph rows; run_command is
 * the bash tool's other name. */
function isShellTool(name: string): boolean {
  return name === 'bash' || name === 'run_command'
}
</script>

<template>
  <div class="tool-row" :class="{ error: row.isError === true }">
    <template v-if="isShellTool(row.name)">
      <span class="shell-mark" aria-hidden="true">$ </span>
      <span class="shell-command mono">{{ row.command ?? '…' }}</span>
      <span
        v-if="typeof row.code === 'number'"
        class="shell-code mono"
        :class="row.code === 0 ? 'ok' : 'failed'"
      >[{{ row.code }}]</span>
      <span v-else-if="row.isError === true" class="shell-code failed">✗</span>
    </template>
    <template v-else>
      <span class="glyph" aria-hidden="true">{{
        row.isError === null ? '…' : row.isError ? '✗' : '✓'
      }}</span>
      <span class="name mono">{{ row.name }}</span>
      <span v-if="row.summary" class="summary mono">{{ row.summary }}</span>
    </template>
  </div>
</template>

<style scoped>
.tool-row {
  display: flex;
  gap: 8px;
  align-items: baseline;
  padding: 5px 10px;
  background: var(--surface);
  border: 1px solid var(--border);
  border-left: 3px solid var(--ok);
  border-radius: 6px;
}

.tool-row.error {
  border-left-color: var(--error);
}

.glyph {
  flex: none;
  color: var(--ok);
}

.tool-row.error .glyph {
  color: var(--error);
}

.name {
  color: var(--text);
}

.summary {
  color: var(--text3);
  overflow: hidden;
  text-overflow: ellipsis;
  white-space: nowrap;
}

/* A bash row reads as the shell line it ran: dim $ prefix, the
   command, and the exit code pushed to the row's right end. */
.shell-mark {
  flex: none;
  color: var(--text3);
  white-space: pre;
}

.shell-command {
  flex: 1;
  min-width: 0;
  overflow: hidden;
  text-overflow: ellipsis;
  white-space: nowrap;
  color: var(--text);
}

.shell-code {
  flex: none;
  font-size: 11px;
}

.shell-code.ok {
  color: var(--ok);
}

.shell-code.failed {
  color: var(--error);
}
</style>
