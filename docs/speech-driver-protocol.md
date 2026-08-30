# Lector speech-host protocol 2.0

Status: project specification

Protocol: `2.0`

Machine-readable definition: [`../openrpc.json`](../openrpc.json)

This document is the normative transport, capability, and state-machine
contract for a Lector speech host. The OpenRPC document is the normative schema
for methods and messages. If the two disagree, this document controls behavior
and transport, while OpenRPC controls JSON shape.

## 1. Conventions and goals

The key words **MUST**, **MUST NOT**, **REQUIRED**, **SHOULD**, **SHOULD NOT**,
and **MAY** are to be interpreted as described by RFC 2119 and RFC 8174.

Version 2 separates three responsibilities:

- Lector owns presentation processing, paragraph splitting, interruption, and
  the pending-utterance queue.
- A host owns one active utterance and adapts a platform speech API.
- Correlated events provide evidence for state transitions. Lector never
  guesses that an utterance finished.

The protocol is designed for small native hosts, Speech Dispatcher adapters,
and hosts written outside this repository. It deliberately exposes semantic
guarantees, not platform API names or native utterance identifiers.

## 2. Selecting and owning a host process

Lector's default `native` host is the running Lector executable in a hidden
server mode. An external host is selected in `init.lua` with an exact argument
vector:

```lua
lector.o.speech = {
  program = "/opt/lector-speech/bin/server",
  args = { "--voice", "Alex" },
}
```

Lector invokes no shell and performs no argument splitting. The direct child
inherits Lector's environment and working directory. It MUST reserve stdin and
stdout for this protocol and SHOULD write diagnostics only to stderr. It MUST
exit promptly on stdin EOF and is responsible for its own descendants.

`lector.api.set_speech(spec)` starts a candidate asynchronously. Lector commits
the setting only after initialization and rate restoration succeed; otherwise
the old host remains selected. An intentional replacement does not count as a
host crash.

## 3. Transport

The transport is UTF-8 NDJSON containing JSON-RPC 2.0 objects:

- stdin carries Lector-to-host requests;
- stdout carries host responses and host-to-Lector notifications;
- each frame is one JSON object followed by LF (`0x0a`);
- an optional CR immediately before LF is accepted;
- literal newlines in strings MUST be JSON-escaped;
- every writer MUST flush after a frame;
- batches and server-to-client requests are not supported; and
- one encoded frame, including LF, MUST NOT exceed 1,048,576 bytes.

Lector permits one outstanding request. A host MAY emit notifications before,
between, or after responses, including while no request is outstanding. It
MUST emit exactly one response for every request with an ID. Notification
traffic does not extend the request deadline.

Lector validates JSON-RPC version, response ID, result/error exclusivity, event
shape, UTF-8, and frame bounds. EOF, malformed framing, an unexpected response,
or a missed deadline is a transport failure. Unknown notification methods,
unknown event types, and additive object members are ignored as described in
section 5; they are not transport failures.

Initialization has a five-second absolute deadline. Ordinary calls have a
one-second absolute deadline. All pipe I/O runs on Lector's speech worker, not
the interactive terminal thread.

### 3.1 Why JSON-RPC over stdio

JSON-RPC provides stable requests, responses, IDs, errors, and mature tooling;
OpenRPC supplies a machine-readable contract. Stdio binds connection lifetime
to the direct child without socket discovery or authentication. Measured JSON
serialization and pipe round trips are small relative to native speech startup
and do not run on the input thread. A binary protocol or dynamic library ABI
would add compatibility and crash-containment costs without improving the
state model.

NDJSON is Lector-specific because JSON-RPC does not define stream framing.

## 4. Initialization and version negotiation

`rpc.discover` MAY be called before initialization. The first other method MUST
be `initialize`, exactly once:

```json
{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocol":{"major":2,"minimumMinor":0,"maximumMinor":0},"client":{"name":"lector","version":"0.4.1"},"clientCapabilities":{"speechEvents":true,"progressModes":["marker","utf8ByteOffset"]}}}
```

The host selects a minor version within the offered range:

```json
{"jsonrpc":"2.0","id":1,"result":{"protocol":{"major":2,"minor":0},"server":{"name":"example-speech","version":"2.4.0"},"capabilities":{"lifecycle":{"started":{"delivery":"reliable"},"terminal":{"delivery":"reliable","distinguishes":["completed","cancelled","failed"]}},"progress":{"modes":[{"kind":"utf8ByteOffset","granularity":["word"]}]},"controls":{"stop":"confirmed","pauseResume":"restartFromWord"},"settings":{"rate":"readWrite"}}}}
```

The major version identifies an incompatible contract. Minor versions are
backward-compatible additions. A host SHOULD select the highest mutually
supported minor. With no compatible version it MUST return error `-32001`.
Names and versions MUST be nonempty.

Protocol 2 clients MUST accept unknown object members, capability families,
capability values, event types, terminal reasons, and progress-position kinds.
An unknown or omitted capability is treated as unsupported. An unknown event
or position MUST NOT change playback state. Extensions SHOULD use a stable,
vendor-qualified key when a collision is plausible.

JSON strings are Unicode. Byte positions are always offsets into the UTF-8
encoding of the exact `speech.speak.text`. UTF-16 code-unit indexes MUST NOT
cross the host boundary. A platform using UTF-16 internally MUST convert at
the adapter and reject positions inside a surrogate pair.

## 5. Capabilities

Capabilities describe independently usable evidence and controls. A host MUST
advertise only behavior it provides for every accepted utterance in that
process generation. Version 2 requires `controls.stop` to be `confirmed` or
`bestEffort`, because cancellation, replacement, and whole-utterance pause
fallback are fundamental. All other capability families are optional.

### 5.1 Lifecycle

`lifecycle.started.delivery` and `lifecycle.terminal.delivery` are one of:

- `reliable`: the event is emitted exactly as required below;
- `bestEffort`: the host may omit it; or
- `unsupported`: callers cannot depend on it.

Unknown values mean `unsupported`. If terminal delivery is `reliable`, every
accepted utterance MUST produce exactly one `ended` event unless the transport
dies. This guarantee is what permits Lector to submit its next queued
utterance. `terminal.distinguishes` lists reasons the host can report
accurately; currently conventional values are `completed`, `cancelled`, and
`failed`. Unknown reasons still terminate the utterance.

### 5.2 Progress

`progress.modes` is a list of independent position encodings and granularities.
Version 2 defines:

- `{"kind":"utf8ByteOffset","granularity":["word"]}`: `offset` is a UTF-8
  character boundary at the beginning of the word being spoken; and
- `{"kind":"marker",...}`: `id` is an opaque marker identifier from a text
  representation understood by both peers.

A mode is usable only when both peers advertise it. Unknown kinds and
granularities are ignored.

### 5.3 Controls

`controls.stop` is `confirmed`, `bestEffort`, or `unsupported`. `confirmed`
means that a successful `speech.stop` response is evidence that playback of
that utterance will not continue. `bestEffort` means a stop was requested but
the backend cannot confirm the outcome.

`controls.pauseResume` is `restartFromWord` or `unsupported`.
`restartFromWord` is valid only with word-granularity UTF-8 byte offsets. It
means:

1. `speech.pause` stops audio and returns the beginning of the word active at
   the pause boundary;
2. the logical utterance and ID remain active but paused; and
3. `speech.resume` starts again at that exact word boundary.

This restart may repeat part of a word; it MUST NOT skip the remainder of the
word. Unknown modes are unsupported.

### 5.4 Settings

`settings.rate` is `readWrite`, `writeOnly`, or `unsupported`. `readWrite`
means `speech.setRate` returns the effective value. `writeOnly` exists for
adapters that can apply but not independently inspect the value. Version 2
Lector currently invokes `speech.setRate` for either advertised mode.

## 6. Common data types

An `utteranceId` is a nonempty opaque JSON string of at most 128 UTF-8 bytes.
Clients MUST NOT encode it as a JSON number. Hosts MUST echo it byte-for-byte
in commands and events and MUST NOT expose a platform-native identifier. The
client MUST NOT reuse an ID during a host session, so a late event can never be
mistaken for evidence about newer speech.

An event `sequence` is a nonnegative integer no greater than
9,007,199,254,740,991. It MUST increase strictly for one utterance. It need not
be consecutive and has no ordering meaning across utterances. Lector ignores a
duplicate or older sequence number.

JSON-RPC request and response IDs are also nonnegative safe integers in this
range. They correlate transport calls only and are unrelated to `utteranceId`.

A UTF-8 position has this form:

```json
{"kind":"utf8ByteOffset","offset":6}
```

`offset` MUST be no greater than the original text's byte length and MUST be a
UTF-8 character boundary. An invalid position is not usable resume evidence.

## 7. Methods

All parameters use JSON-RPC by-name objects.

### 7.1 `rpc.discover`

This method takes no parameters and returns an OpenRPC document compatible
with [`../openrpc.json`](../openrpc.json). It is available before and after
initialization. OpenRPC describes JSON shapes; this document still controls
stdio framing, deadlines, process ownership, and recovery.

### 7.2 `speech.speak`

```json
{"jsonrpc":"2.0","id":2,"method":"speech.speak","params":{"utteranceId":"41:0","text":"first paragraph"}}
{"jsonrpc":"2.0","id":2,"result":{"accepted":true}}
```

The host MUST reject this method if another logical utterance is active. A
successful response transfers responsibility for exactly this utterance to
the host but does not imply playback started or finished. The host MUST NOT
queue another Lector utterance internally. Lector submits the next one only
after reliable terminal evidence.

The public Lector speech layer assigns a logical ID. A single line boundary is
normalized to a space before transport. A run of two or more CR/LF line
boundaries is a paragraph boundary; each nonempty paragraph becomes a separate
utterance with a stable, opaque child ID and is sequenced by Lector. The Lua
`lector.api.speak` call returns the parent logical ID; hosts see and echo only
the individual child IDs and MUST NOT infer their relationship from their
format. CRLF counts as one line boundary. Lector waits on a nonblocking 200 ms
deadline after the preceding paragraph's reliable terminal event before it
submits the next paragraph. The delay is presentation policy, not a host timer
or protocol field.

If reliable terminal delivery is unavailable, Lector cannot know when to
submit the next paragraph. It therefore replaces paragraph boundaries with
spaces and submits the logical request as one utterance instead of guessing.

### 7.3 `speech.stop`

```json
{"jsonrpc":"2.0","id":3,"method":"speech.stop","params":{"utteranceId":"41:0"}}
{"jsonrpc":"2.0","id":3,"result":{"accepted":true}}
```

The call is idempotent when no utterance is active. If the supplied ID does not
identify the active utterance, the host SHOULD return `-32602`. A reliable
lifecycle host MUST emit one `ended` event with a cancellation or failure
reason unless it already emitted the terminal event.

This is a host primitive, not a declaration of Lector's queue policy. Lector
uses it for cancellation, replacement, and whole-utterance pause fallback. The
client-side transition determines whether the current item and pending queue
are discarded or retained.

### 7.4 `speech.pause`

```json
{"jsonrpc":"2.0","id":4,"method":"speech.pause","params":{"utteranceId":"41:0"}}
{"jsonrpc":"2.0","id":4,"result":{"paused":true,"position":{"kind":"utf8ByteOffset","offset":6}}}
```

Only a host advertising resumable pause implements this method. On success,
`paused: true` MUST include a valid position satisfying the advertised mode.
`paused: false` means there is no resumable paused utterance and MUST omit the
position. Lector conservatively follows `paused: false`, an RPC failure, or an
invalid position with `speech.stop`. It retains the stopped text and its
never-submitted queue; after confirmed stop or reliable terminal evidence, a
later resume resubmits that complete utterance under a fresh opaque ID.
Repeating pause while already paused is idempotent.

### 7.5 `speech.resume`

```json
{"jsonrpc":"2.0","id":5,"method":"speech.resume","params":{"utteranceId":"41:0"}}
{"jsonrpc":"2.0","id":5,"result":{"accepted":true}}
```

The host resumes the same logical utterance according to its advertised mode.
For `restartFromWord`, it resynthesizes the original text beginning at the
position returned by `speech.pause`; later UTF-8 progress positions remain
relative to the original complete text. It MUST reject an ID that is not the
paused utterance. If resume fails, Lector stops the uncertain utterance and
retains the complete text for the same fresh-ID restart fallback.

### 7.6 `speech.setRate`

```json
{"jsonrpc":"2.0","id":6,"method":"speech.setRate","params":{"rate":1.25}}
{"jsonrpc":"2.0","id":6,"result":{"rate":1.25}}
```

`rate` MUST be finite and uses the host backend's documented domain. The host
MAY clamp it and returns the finite effective value. Lector restores this
value when replacing a host process.

## 8. `speech.event` notifications

Notifications have no ID:

```json
{"jsonrpc":"2.0","method":"speech.event","params":{"utteranceId":"41:0","sequence":0,"event":{"type":"started"}}}
{"jsonrpc":"2.0","method":"speech.event","params":{"utteranceId":"41:0","sequence":1,"event":{"type":"progress","position":{"kind":"utf8ByteOffset","offset":6}}}}
{"jsonrpc":"2.0","method":"speech.event","params":{"utteranceId":"41:0","sequence":2,"event":{"type":"ended","reason":"completed"}}}
```

Defined event types are:

- `started`: playback began for the logical utterance;
- `progress`: playback reached `position`;
- `paused`: playback is paused at `position`;
- `resumed`: playback resumed, optionally with `position`; and
- `ended`: the sole terminal event, with nonempty `reason` and an optional
  human-readable `message`.

Events for an unknown ID, an already-ended ID, an older process generation, or
a non-increasing sequence number MUST NOT advance Lector's queue. Unknown event
types are ignored. A host MUST preserve event order on stdout.

## 9. Lector playback and queue semantics

Lector's manager has at most one host-active utterance and a queue of
never-submitted utterances. The queue retains at most 32 utterances and 256 KiB
of text; adding beyond either limit evicts the oldest pending utterances. Its
evidence-backed states distinguish natural idle, active playback, explicit
suspension, replacement, cancellation, paragraph delay, and suspension before
the next paragraph.

- Reliable `ended` evidence transitions active playback to the next queued
  item.
- A non-interrupting public `speak` appends while logical playback is active.
  A paragraph delay is active playback even though it is temporarily silent.
- A non-interrupting `speak` received while explicitly suspended discards the
  retained current item and its queue, then starts the new request. At natural
  idle it simply starts the new request.
- An interrupting `speak` always clears retained speech and replaces it with
  the new request, whether playback is active or suspended.
- `pause_speaking` suspends in one direction, `resume_speaking` resumes in one
  direction, and `toggle_speaking` selects between them. They are idempotent
  where applicable and do nothing at natural idle. Typing and other ordinary
  input invoke the one-way pause transition. `M-x` invokes the toggle action;
  `M-X` has no default speech binding.
- `cancel_speaking` clears the active or paused item and every pending item,
  then stops host playback. It is deliberately unbound by default.
- If resumable pause is advertised, suspension retains the item and resume
  restarts at the beginning of the interrupted word.
- Otherwise, suspension stops and retains the complete active utterance. Once
  a confirmed stop response or reliable terminal event proves that old audio
  is gone, resume resubmits it from the beginning with a fresh opaque ID. A
  resume requested while a best-effort stop is awaiting its terminal event is
  remembered. Pending announcements and paragraphs remain queued.
- During the paragraph-delay state, suspension freezes the queue without
  contacting the host. Resume immediately starts the first word of the next
  paragraph.
- A missing, unknown, out-of-range, or non-UTF-8 pause position is
  conservatively handled by the same whole-utterance restart fallback instead
  of guessing a word boundary.

If terminal delivery is not reliable, Lector cannot safely sequence a second
version 2 utterance and rejects that ambiguous queue transition. Other basic
speech remains available. Version 1 hosts retain their historical internal
queue as a compatibility exception.

## 10. Errors, failure recovery, and shutdown

Standard JSON-RPC errors apply:

| Code | Meaning |
| ---: | --- |
| `-32700` | Parse error |
| `-32600` | Invalid JSON-RPC request or state transition |
| `-32601` | Method or capability not supported |
| `-32602` | Invalid parameters or utterance ID |
| `-32603` | Internal speech-backend failure |
| `-32001` | No compatible Lector speech protocol version |

A well-formed RPC error is an operation failure, not automatically a process
crash. A transport failure terminates and reaps that process generation. An
in-flight utterance is never replayed because the host might have performed it;
only queued utterances that were never submitted may survive replacement.

Startup is attempted twice. At runtime, Lector allows one automatic restart in
a rolling 30-second crash interval; a second failure inside the interval, or a
failed replacement startup, is fatal. On normal shutdown Lector terminates and
reaps the direct child. The built-in host also watches its parent PID. Custom
hosts SHOULD use stdin EOF or an equivalent parent-death guard around blocking
foreign APIs.

## 11. Version 1 migration

Lector first offers version 2. If `initialize` returns `-32001`, it retries the
published version 1.0 initialization and old method names (`speak`, `stop`, and
`set_rate`). If `initialize` returns `-32601`, it treats the process as an
unversioned legacy host.

Legacy hosts have no correlated lifecycle or progress evidence, do not support
word-position resume, and retain backend-owned queueing. Lector can request a
stop and retain its currently tracked utterance, but an unversioned host
supplies no safe completion evidence, so Lector never guesses that
resubmission is safe. A later new speech request may replace the suspended
item, but resume can remain unavailable. Lector also cannot reliably
reconstruct other items already accepted into an opaque legacy queue.
Multi-paragraph logical requests are therefore flattened into one utterance
for legacy hosts. New
implementations MUST implement version 2 and MUST NOT depend on the
unversioned escape hatch.

## 12. Implementation checklist

A conforming version 2 host must:

1. Reserve stdin/stdout for bounded UTF-8 NDJSON JSON-RPC and flush frames.
2. Implement version-range initialization, discovery, and explicit nested
   capabilities; omit or mark unsupported anything it cannot guarantee.
3. Accept at most one active Lector utterance and echo its opaque string ID.
4. Emit strictly sequenced, correlated events exactly as advertised.
5. Translate native indexes to markers or valid UTF-8 byte boundaries.
6. Implement pause/resume only if it restarts the interrupted word; otherwise
   advertise it as unsupported and provide stop for Lector's restart fallback.
7. Respond inside the deadlines, exit on EOF, and clean up descendants.
