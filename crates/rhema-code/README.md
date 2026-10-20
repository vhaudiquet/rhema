# Rhema Code

```
curl -fsSL https://get.rhema.rs/code | sh
rhema
```

Rhema Code is a terminal-based coding agent, built on the Rhema agent framework.

<img align="center" src="../../.github/img/rhema-code.png">
<div align="center"><em>The Rhema Code TUI.</em></div>

## Features

### Rich settings

<!-- TODO Screenshot -->

### Colors

The look and feel is machine-owned: it lives in `config.yaml`, not in flags. The `colors` key picks the palette — the setting defaults to `rhema` and cycles live from the Appearance page of `/settings`:

```yaml
colors: rhema          # rhema | aquamarine | black-and-white | dracula | catppuccin-* | m3-you | custom
theme: dark            # dark | light — the palette's appearance
icons: unicode         # unicode | nerd — the footer's icon flavor
m3_source: "#6750A4"   # the m3-you seed color (#RRGGBB), optional
custom:                # the custom palette's tokens (#RRGGBB), optional
  dark:
    ground: "#1E1E2E"
    accent: "#89B4FA"
    # ... every token name; anything left out reads as the rhema default
  light:
    ground: "#EFF1F5"
```

Every palette ships both appearances. Rhema is the design the TUI was drawn against; Aquamarine and Black-and-white restyle the same slots; Dracula maps the official colors onto them (its light side is derived, since Dracula ships no official light variant). The Catppuccin flavors (`catppuccin-frappe`, `catppuccin-macchiato`, `catppuccin-mocha`) use the official hexes in the roles their [style guide](https://github.com/catppuccin/catppuccin/blob/main/docs/style-guide.md) defines; `theme: light` reads Latte, the set's only light flavor, since the guide forbids blending flavors inside one theme. M3 You generates a [Material 3](https://m3.material.io/styles/color/system/overview) tonal-spot scheme from a seed color: set `m3_source` to a color picked from your wallpaper (a terminal cannot see it), and the whole UI — grounds, text, accent, status colors — derives from it. Without a seed it uses Material's baseline purple.

`custom` reads every color from the config, which is the hook for external generators: a wallpaper watcher or scheme tool (pywal, [matugen](https://github.com/InioX/matugen), a shell one-liner) computes a scheme and writes the `custom` block, and rhema picks it up on the next start. Tokens are named after the design slots (`ground`, `surface`, `text`, `accent`, ... — all seventeen); anything left out, or written in a form that is not a `#RRGGBB` color, reads as the rhema default for that slot, so a file overriding two tokens is still a valid palette. An appearance the file does not give reads as the other one's full scheme, so a generator writing only `dark` still paints on light terminals.

### Customizable, scriptable system-prompt

```
rhema system edit
```

The above command allows you to edit the system prompt in your `$EDITOR`. It uses a Jinja template (`minijinja`), so you can customize the skills part, the tool part, etc.

### ACP (Agent Client Protocol) support

You can use Rhema Code directly in your editor or orchestrator, using the Agent Client Protocol.

```sh
rhema acp
```

An editor launches the above command and directly talks ACP with Rhema Code.

### Profiles

Rhema Code ships with two default profiles `work` and `personal` that allows you to manage your different subscription contexts. Each profile can store any amount of subscription instances.

Login keys are stored in your system keychain (macOS Keychain, Windows Credential Manager, or the freedesktop Secret Service) whenever one is reachable — the config file then holds only an opaque reference to the entry — and fall back to the file itself on machines without one (containers, headless boxes). Logging in again replaces the keychain entry; forgetting the instance deletes it. Environment variables still outrank everything stored.

### Skills, sessions, etc.

Regular coding agent features are supported, while maintaining a simplicity balance. Right now, no MCPs.

### Subagents

Drop a folder holding a `SUBAGENT.md` into `.rhema/subagents/` in your workspace (or `~/.config/rhema/subagents/` for every workspace; project shadows global), and the agent gains one `subagent` tool. The frontmatter `description` is the trigger the agent sees, the body is the subagent's own system prompt, and `model` / `max_steps` are optional:

```markdown
---
description: Reviews a diff for correctness. Use for a second opinion before committing.
model: anthropic/claude-haiku
max_steps: 128
---

You are a code reviewer. ...
```

When the agent delegates, the subagent runs its own conversation with the same workspace tools in the same workspace, its output streams into the tool-call view, and its final report lands in the conversation as the tool's result. Inner commands ask for approval exactly like the main agent's do. A subagent never spawns further subagents.

### Program status (OSC 7501)

Rhema Code reports what it is doing to the terminal, following [Mitchell Hashimoto's program status protocol](https://mitchellh.com/writing/program-status-osc7501): `working` (with step progress and the current tool) while a turn runs, `blocked` (permission or auth) when it needs you, `idle` at the prompt, and `done` or `error` when a `rhema run` one-shot finishes — so supporting terminals (Ghostty, and terminal UIs built on libghostty) can mark tab headers, session pickers, and agentic inboxes without scraping the screen. Terminals without support ignore the sequence; piped output stays byte-exact.

## Documentation

## About

```
MIT License
Copyright (c) 2026 Valentin Haudiquet
```