---
name: commit-message
description: How to write a good commit message. Triggers when needing to commit.
---

# Commit message

Write a commit message.

## Repository commit conventions

Check the repository for AGENTS.md, CONTRIBUTING.md, .gitmessage, etc.
If they describe a specific commit convention for the repo, follow them.
Otherwise, run `git log -n 20 --format='%s%n%n%b%n---'` to look at the last 20 commits
and infer the commit convention.

The important thing is that a commit should always blend in with
the repository existing conventions/history.

## Inferring convention from history

Look at the subjects and bodies and note:
- Prefix scheme: `feat:` / `fix:` / `[TICKET-123]` / none.
- Casing and mood: capitalized? imperative ("add X") or past ("added X")?
- Subject length: roughly how long are they?
- Bodies: used at all? Prose, bullets, `Why:`/`What:` sections, trailers?

If a hint is provided by the user, it overrides everything above.

## Default when undecidable

If the repo has no convention and history is inconclusive:
- Imperative mood ("add", "fix", "remove" and not "added", "adds").
- Subject ≤72 characters, ideally ≤50.
- Body only when the *why* isn't obvious from the subject.

## Anti-slop

DO NOT use:
- "This commit...", "I've...", "We..."
- Filler: "minor changes", "various improvements", "as requested", 
  "some cleanup", "misc".
- Emoji, unless the repo's history uses them.
- Bullet lists in the subject line.

For the commit text itself, use *unslop* skill.
