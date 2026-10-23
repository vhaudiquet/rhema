---
description: Push a notification to the user's devices — reminders and heads-up messages that reach them outside this chat, delivered at the device's next poll
---

# notify

`notify` queues one notification on the daemon: a device polls for it and
acks it on delivery. Use it when the user must hear something when this
conversation is not in front of them — "remind me", "tell me when X", or a
finding from a scheduled turn.

## Push one notification

```bash
notify push --title "CI is red" --body "acme/widgets: main failed on unit tests"
```

Add `--device <name>` to target a device other than the default phone.

The answer is the queued row as one JSON line; `"id"` addresses it on the
daemon (`DELETE /v1/notifications/{id}` over the API, by a human client).

## Notes

- Failures print `{"error": "..."}` and exit non-zero: exit 2 means the
  daemon is unreachable (`RHEMA_URL`), 3 means the credential was refused.
- The queue is at-least-once: an unacked row keeps coming back until the
  device acks it. Do not re-push to "make sure" — ask the user first.
