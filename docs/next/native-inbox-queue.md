# Guarded native inbox queue

The optional `agent.queue_prompt` JSON API method submits text to a verified
Claude native session using the existing prompt transport. It accepts a current
`working`, `idle`, or `done` state. It never sends Escape or a cancellation key.
This lets a coordination client explicitly queue a message during a tool run.

```sh
herdr agent queue-prompt '{"target":"w1:p1","terminal_id":"term_ID","native_id":"NATIVE_ID","state_change_seq":2,"text":"Read the named inbox message after the current tool."}'
```

Read the binding and `state_change_seq` from `agent.get` immediately before the
request. All fields are required; the target must be the explicit pane ID. The
runtime rejects a changed terminal, conversation, or lifecycle sequence, unknown
or blocked state, pending launch, inactive foreground process, unsupported agent,
empty/oversized text, and terminal control characters. Only Claude is supported.

A successful `agent_prompted` response confirms completion of the PTY transport,
not native acceptance or message acknowledgement. The calling service must commit
its once-only intent before this method, own request-key deduplication, and retain
uncertain results without resending. There is no additional Herdr queue, poller,
or durable coordination store. The existing `agent.prompt` contract is unchanged.

Consumers detect this optional method through the installed API schema and
compatible running server. Missing support disables this action only.
