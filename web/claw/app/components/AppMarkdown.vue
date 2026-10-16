<script setup lang="ts">
// Agent replies are markdown; this mounts the rendered HTML. The parser
// keeps raw HTML in the source escaped (html stays false) and refuses
// javascript: link targets, so v-html only ever receives generated
// markup.
import MarkdownIt from 'markdown-it'
import type { RendererRule } from 'markdown-it'

const props = defineProps<{
  text: string
  /** While true, a blinking cursor trails the last rendered block. */
  streaming?: boolean
}>()

const md = new MarkdownIt({ html: false, linkify: true })

// Rendered links leave the dashboard; open them in a new tab so the
// transcript and its event stream survive the click.
const defaultLinkOpen: RendererRule =
  md.renderer.rules.link_open ??
  ((tokens, idx, options, env, self) => self.renderToken(tokens, idx, options))
md.renderer.rules.link_open = (tokens, idx, options, env, self) => {
  const token = tokens[idx]
  if (token) {
    token.attrSet('target', '_blank')
    token.attrSet('rel', 'noopener noreferrer')
  }
  return defaultLinkOpen(tokens, idx, options, env, self)
}

const html = computed(() => md.render(props.text))
</script>

<template>
  <div class="md" :class="{ streaming }" v-html="html" />
</template>
