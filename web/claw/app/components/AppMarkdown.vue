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

// Streaming re-parses the whole reply per delta — quadratic over the
// reply's length, and every render lands on the main thread. A short
// trailing debounce keeps the render at a readable cadence while the
// turn streams; a settled text renders at once, so a finished reply is
// never a frame behind its last delta.
const html = ref('')
let renderTimer: ReturnType<typeof setTimeout> | null = null

watch(
  () => props.text,
  (text) => {
    if (renderTimer !== null) clearTimeout(renderTimer)
    if (!props.streaming) {
      html.value = md.render(text)
      return
    }
    renderTimer = setTimeout(() => {
      renderTimer = null
      html.value = md.render(props.text)
    }, 80)
  },
  { immediate: true },
)

onUnmounted(() => {
  if (renderTimer !== null) clearTimeout(renderTimer)
})
</script>

<template>
  <div class="md" :class="{ streaming }" v-html="html" />
</template>
