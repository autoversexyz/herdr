# Guarded native prompt replies

The optional JSON API method `agent.prompt_answer` lets a client inspect a blocked
native menu and relay an externally authorized choice. Herdr supplies terminal
interpretation and guarded input; the caller owns authorization, durable audit,
retry custody and human interaction.

```sh
herdr agent prompt-answer '{"target":"w1:p1"}'
herdr agent prompt-answer '{"target":"w1:p1","expected_prompt":"TOKEN_FROM_PREVIEW","option":"2"}'
```

For a blocked pending launch without a native session, both calls require the
exact public pane ID as `target` and its matching `terminal_id`. The response
reports `startup: true` and the binding contains `native_session: null`.

```sh
herdr agent prompt-answer '{"target":"w1:p1","terminal_id":"term_1"}'
herdr agent prompt-answer '{"target":"w1:p1","terminal_id":"term_1","expected_prompt":"TOKEN_FROM_PREVIEW","option":"2"}'
```

The `native_prompt` response contains the dialog text, options, binding and
fingerprint. Established conversations require a blocked agent with a native
session and report `startup: false`. In both cases Herdr verifies the foreground
agent. The dialog region starts after the last horizontal rule when one exists,
and is limited to 32 lines and 4096 bytes. The fingerprint covers the full
detection screen, pane, terminal, harness, session and state transition.

Codex's `Folder access` startup trust dialog exposes its numbered options `1`
and `2`. Claude's unnumbered startup trust dialog exposes the same indices.
For both dialogs, Apply sends Enter if the requested option is already selected.
Otherwise it sends one arrow key and waits briefly for the marker to reach the
requested row. It sends Enter when the redraw confirms that selection and the
rest of the detection screen and the option labels still match. Each enqueue
checks the verified content revision under the terminal writer lock. If
navigation is queued and confirmation cannot proceed, the API returns
`native_prompt_partial`. Inspect the resulting state before considering another
attempt.

Other numbered menus require at least two distinct single-digit options, and
Apply sends one digit.

`input_queued` records transport acceptance. Native acceptance remains
`unverified` until the caller reads back and reconciles the native state. Clients
must durably reserve their attempt before calling apply and treat timeouts as
ambiguous.

This adds an optional method without changing existing key-input methods or
frozen endpoint codecs. Clients must check the advertised method and installed
server compatibility before using it.
