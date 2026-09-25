# Guarded native prompt replies

The optional JSON API method `agent.prompt_answer` lets a client inspect a blocked
native menu and relay an externally authorized choice. Herdr supplies terminal
interpretation and guarded input; the caller owns authorization, durable audit,
retry custody and human interaction.

```sh
herdr agent prompt-answer '{"target":"w1:p1"}'
herdr agent prompt-answer '{"target":"w1:p1","expected_prompt":"TOKEN_FROM_PREVIEW","option":"2"}'
```

The `native_prompt` response contains the original dialog text, literal options,
native binding and fingerprint. Menus require at least two explicit single-digit
numbered options, no duplicate keys, and a complete detection region within
32 lines / 4096 bytes. The existing `after_last_horizontal_rule` structural helper
separates the dialog from transcript history; when no separator exists it retains
the whole region. The fingerprint still covers the full detection screen.
Other prompts remain manual. No detector rule, default
answer or permission decision is introduced.

Both reads and writes require a blocked identified native conversation and its
foreground process. Apply recomputes the fingerprint from the screen, conversation,
terminal, pane, harness and state transition. It rejects changed evidence or an
unlisted option, encodes one key with the current native keyboard protocol, then
checks the content revision and enqueues under the terminal content writer lock.
No trailing Enter or chat prompt is sent. This code runs only on explicit API
requests; rendering, normal detection and background pane loops add no work.

`input_queued` means the runtime accepted a transport write, not that the native
application accepted the choice. The caller must read back and reconcile native
state. A timeout is ambiguous: never retry a key merely because the response was
lost. Clients must durably reserve their attempt before calling apply. Herdr does
not authenticate a human choice or persist client idempotency keys.

This adds an optional method without changing existing key-input methods or
frozen endpoint codecs. Clients must check the advertised method and installed
server compatibility before using it. There is no unsafe unguarded fallback.
