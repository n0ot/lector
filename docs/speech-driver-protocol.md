# Speech driver protocol

This document is the normative transport and lifecycle contract for a custom
Lector speech server. The repository-root [`openrpc.json`](../openrpc.json) is
the machine-readable method schema. Both describe speech protocol version
`1.0`; when they differ, this document controls transport and lifecycle and the
OpenRPC document controls method shapes.

Lector's default native speech uses this same protocol. It locates its current
executable rather than trusting `argv[0]`, then starts that executable with the
hidden internal `--native-speech-server` mode. There is no separate native TTS
binary to install. The hidden mode is an implementation detail, not a public
command-line interface for selecting speech.

## Selecting a server

Speech is configured in `init.lua`, before Lector starts the selected server:

```lua
-- Default: start Lector's native speech host.
lector.o.speech = "native"

-- Start an external server with an exact argument vector.
lector.o.speech = {
  program = "/opt/lector-speech/bin/server",
  args = { "--voice", "Alex", "--punctuation=some" },
}
```

`program` is passed directly to the operating system and `args` is passed as
its argument vector. Lector never invokes a shell or splits argument strings.
The child inherits Lector's environment and working directory. `args` may be
omitted or empty. Unknown table keys, an empty or NUL-containing program, NUL
arguments, sparse argument tables, and non-string arguments are configuration
errors.

At runtime, `lector.api.set_speech(spec)` requests a transactional asynchronous
switch using either form above. The call validates and queues the newest switch
intent, then returns immediately. Lector retains the old server for rollback
while the candidate starts; later speech waits in the worker's bounded queue
during that handshake. Lector commits the new setting only after initialization
and rate restoration succeed, then terminates and reaps the old server. A
candidate failure resumes through the old server and invokes
`lector.hooks.on_error(message, "speech-reconfigure")`. An intentional switch
neither counts as a server crash nor clears the most recent real crash time
used for restart-rate limiting.

`lector.o.speech` returns the active setting. Assigning it is allowed only in
the top-level configuration phase; assignment from `on_startup`, another hook,
or the Lua REPL is an error directing the caller to
`lector.api.set_speech(spec)`.

## Process and stream ownership

Lector starts one direct child process for a speech-server generation:

- The child's standard input carries requests from Lector.
- The child's standard output carries responses to Lector.
- Standard error is inherited and is not part of the protocol.
- The child must not use standard input or output for prompts, logs, banners,
  or any other data.
- Lector sends one call at a time and waits for its matching response before it
  sends another. It does not send JSON-RPC notifications or batch requests.
- The child must stop accepting work and exit promptly when standard input
  reaches EOF. A custom server is responsible for cleaning up any descendants
  it creates.

The transport is UTF-8 NDJSON carrying JSON-RPC 2.0 objects. Each request or
response is exactly one JSON object followed by LF (`0x0a`). Newlines inside a
string are JSON escapes, never literal framing bytes. Lector emits LF and
accepts an optional CR immediately before LF. A server must flush standard
output after every response.

One encoded frame, including its terminating newline, must not exceed 1 MiB
(`1,048,576` bytes). An oversized response is a transport failure. Lector
bounds one speech announcement to 64 KiB before JSON encoding; a local request
that still cannot fit is rejected rather than partially written.

Lector validates every response strictly. It must be a JSON-RPC 2.0 response
with the outstanding unsigned integer `id`, and it must contain exactly one of
`result` or `error`. EOF, pipe errors, invalid UTF-8 or JSON, an invalid
envelope, a wrong version or ID, an oversized frame, and a missed deadline are
transport failures. A well-formed JSON-RPC `error` response is an operation
failure, not by itself a process crash or restart trigger.

### Why JSON-RPC over stdio

JSON-RPC gives custom servers a stable request, response, ID, and error model
with mature tooling, while OpenRPC supplies machine-readable method schemas.
Stdio ties the transport lifetime directly to the child and needs no socket
path, listener, authentication, or connection race. Speech synthesis dominates
the small serialization cost, and all serialization and pipe I/O are already
off the interactive thread. A bespoke binary protocol would add compatibility
and debugging cost without improving terminal latency.

NDJSON is the deliberately small Lector-specific part: JSON-RPC itself does
not define stream framing. That is why the LF boundary, byte limit, deadlines,
and process lifecycle are specified here even though the methods are
self-describing through OpenRPC.

## Session initialization

On every new process, Lector's first call is `initialize`:

```json
{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocol_version":"1.0","client":{"name":"lector","version":"0.4.0"}}}
```

A compatible server responds within five seconds:

```json
{"jsonrpc":"2.0","id":1,"result":{"protocol_version":"1.0","server":{"name":"example-speech","version":"2.4.0"},"capabilities":{"speak":true,"stop":true,"set_rate":true,"rpc_discover":true}}}
```

The version must equal `1.0`, the server name and version must be nonempty, and
all four version 1.0 capabilities must be `true`. An incompatible result makes
that process-generation startup fail. A server must not perform speech
operations before initialization succeeds; if one is received, it rejects the
call as an invalid request (`-32600`) without invoking the speech backend.

For migration, Lector accepts JSON-RPC method-not-found (`-32601`) from
`initialize` as an unversioned legacy server. New servers must not rely on this
escape hatch: it provides no readiness, compatibility, or discovery guarantee
and may be removed in a future protocol version.

After initialization, every ordinary call has a one-second absolute deadline.
Deadlines and pipe readiness live exclusively on the speech worker; they add no
polling cadence or wait to Lector's terminal event loop. A response arriving
early wakes the worker immediately.

## Discovery

Protocol 1.0 servers implement `rpc.discover` with no parameters. It returns an
OpenRPC document compatible with [`openrpc.json`](../openrpc.json). The method
is available before and after `initialize`, so a protocol tool can inspect a
server without starting speech:

```json
{"jsonrpc":"2.0","id":7,"method":"rpc.discover"}
```

OpenRPC documents method names and JSON shapes. It does not define this
stdio/NDJSON transport, deadlines, process ownership, or recovery policy, so
server authors still need this document. The `x-lector-transport` extension in
the canonical OpenRPC document provides a machine-readable summary of those
bounds.

## Speech methods

All parameters use JSON-RPC's by-name object form.

### `speak`

```json
{"jsonrpc":"2.0","id":2,"method":"speak","params":{"text":"build complete","interrupt":false}}
{"jsonrpc":"2.0","id":2,"result":null}
```

The response acknowledges acceptance; it does not wait for playback to finish.
With `interrupt: true`, the server stops active speech and discards speech it
has queued before accepting the new text. With `false`, it preserves speech
order. Lector also bounds its own pending speech queue and may discard stale
announcements before they reach the server.

### `stop`

```json
{"jsonrpc":"2.0","id":3,"method":"stop"}
{"jsonrpc":"2.0","id":3,"result":null}
```

The server stops active playback and discards speech it has queued.

### `set_rate`

```json
{"jsonrpc":"2.0","id":4,"method":"set_rate","params":{"rate":1.25}}
{"jsonrpc":"2.0","id":4,"result":{"rate":1.25}}
```

`rate` is a finite JSON number in the backend's rate domain. A server may clamp
it to its supported range and returns the finite effective value. Lector
restores the configured rate on a replacement process before routing new
speech to it.

Servers use the standard JSON-RPC error codes for parsing, envelopes, methods,
parameters, and internal failures:

| Code | Meaning |
| ---: | --- |
| `-32700` | Parse error |
| `-32600` | Invalid JSON-RPC request |
| `-32601` | Method not found |
| `-32602` | Invalid method parameters |
| `-32603` | Internal speech-backend error |
| `-32001` | Unsupported Lector speech protocol version |

An error response uses the request ID, except that a parse or envelope error
whose ID is unavailable uses `null` as required by JSON-RPC 2.0.

## Failure recovery and shutdown

Starting the selected server is part of Lector startup. Lector spawns and
initializes it; if that attempt fails, Lector terminates and reaps the process
and retries once with a fresh process. If the retry fails, Lector aborts setup
with a clear error rather than running silently without speech.

During normal operation, a transport failure records a monotonic crash time,
terminates and reaps the failed generation, and starts a fresh generation. An
in-flight request is never replayed because the failed server may already have
performed it. The configured rate is restored before subsequent speech is
accepted.

Only one automatic restart is allowed in a rolling 30-second crash interval.
If there is no previous crash, or the previous crash was at least 30 seconds
ago, Lector records the new time and attempts a restart. A second failure less
than 30 seconds after the recorded crash, or any failure to spawn, initialize,
or restore a restarted server, makes Lector leave its event loop and exit
nonzero. Exactly 30 seconds is eligible for a restart. This is a timestamp
comparison performed only on failure, not a periodic timer.

Runtime terminal input, rendering, direct PTYs, ordinary tmux, and tmux control
mode continue while a speech call is pending. Even a server stuck in native
code therefore cannot hang Lector as a whole: the speech-worker deadline turns
the hang into the failure policy above and wakes the main loop through its
event-driven control path.

On an ordinary exit Lector requests speech-worker shutdown, terminates the
direct speech child if necessary, and reaps it. On handled termination signals,
including one received during the startup handshake, Lector performs speech
and terminal teardown before re-raising the signal. The built-in native host
also watches its parent PID so it exits after an abrupt Lector death for which
destructors or signal cleanup cannot run. Custom servers must additionally
treat stdin EOF as their parent-death indication; Lector does not own or clean
up grandchildren created by a custom server.

No parent can run cleanup after an uncatchable `SIGKILL`. The operating system
still closes Lector's pipe ends, and the built-in watchdog supplies a second
parent-death check, but a custom server that is permanently stuck outside its
read loop or deliberately ignores EOF can remain orphaned. Custom server
authors should add their own parent-death watchdog when they call blocking
foreign code.

## Compatibility checklist

A version 1.0 custom server must:

1. Reserve stdin and stdout for 1 MiB-bounded UTF-8 NDJSON JSON-RPC 2.0.
2. Respond to `initialize` within five seconds with every required capability.
3. Implement `rpc.discover`, `speak`, `stop`, and `set_rate` with the schemas in
   `openrpc.json` and respond to ordinary calls within one second.
4. Flush each response, preserve non-interrupting speech order, and make
   `stop` and interrupting speech discard its own queued work.
5. Exit promptly on stdin EOF and own the lifecycle of any descendants.

Protocol evolution changes `protocol_version` and the `info.version` in
`openrpc.json`. Additive documentation corrections do not change the wire
version; incompatible method, schema, or lifecycle changes do.
