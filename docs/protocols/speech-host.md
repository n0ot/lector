# Speech-host protocol

This is the protocol between Lector and a speech server, called a *host*
here. Implement it to make Lector speak through any speech engine, on any
machine.

- **Version:** 2.2. Hosts stay backward compatible with 2.0 and 2.1.
- **Schema:** [`crates/lector-tts/openrpc.json`](../../crates/lector-tts/openrpc.json)
  defines the JSON shape of every method and message.
- **This document** defines transport, capabilities, and behavior. If the two
  disagree, this document wins on behavior and transport, and the schema wins
  on JSON shape.

The words **MUST**, **MUST NOT**, **SHOULD**, **SHOULD NOT**, and **MAY** have
their RFC 2119 and RFC 8174 meanings.

If you only want to *configure* speech, read the
[speech guide](../guide/speech.md) instead.

## Contents

1. [Overview](#1-overview)
2. [The host process](#2-the-host-process)
3. [Transport](#3-transport)
4. [Initialization](#4-initialization)
5. [Capabilities](#5-capabilities)
6. [Data types](#6-data-types)
7. [Methods](#7-methods)
8. [Events](#8-events)
9. [How Lector queues speech](#9-how-lector-queues-speech)
10. [Errors, recovery, and shutdown](#10-errors-recovery-and-shutdown)
11. [Older protocol versions](#11-older-protocol-versions)
12. [Implementation checklist](#12-implementation-checklist)

## 1. Overview

The protocol is JSON-RPC 2.0, one JSON object per line, over the host's
standard input and output.

Responsibilities are split three ways:

- **Lector** owns text processing, splitting into paragraphs, interruption,
  and the queue of speech not yet sent.
- **The host** owns exactly one active utterance and adapts a platform speech
  API.
- **Events** from the host are the evidence for every state change. Lector
  never guesses that an utterance finished.

The protocol exposes guarantees, not platform API names or native utterance
identifiers.

## 2. The host process

Lector starts the host as a direct child process, using the program and exact
argument list from the user's configuration. No shell is involved and no
arguments are split. The child inherits Lector's environment and working
directory.

A host:

- MUST reserve stdin and stdout for this protocol;
- SHOULD write diagnostics only to stderr;
- MUST exit promptly when stdin reaches end of file; and
- is responsible for cleaning up any processes it starts.

Because the transport is stdio, the configured program may be a bridge such
as `ssh` that runs the real host elsewhere. Lector does not manage or secure
that bridge.

Choosing a speech engine ("backend") is the host's own concern, decided by
its command-line options. The built-in host accepts `--backend ID`, and
`--list-backends` and `--list-voices` to inspect what is available. The host
reports the backend it chose during initialization, so Lector never has to
infer it.

## 3. Transport

Messages are UTF-8 JSON-RPC 2.0 objects, one per line (NDJSON):

- stdin carries requests from Lector to the host;
- stdout carries the host's responses and notifications;
- each frame is one JSON object followed by LF (`0x0a`);
- a CR immediately before the LF is accepted;
- newlines inside strings MUST be JSON-escaped;
- every writer MUST flush after each frame;
- batches and host-to-Lector requests are not supported; and
- one frame, including its LF, MUST NOT exceed 1,048,576 bytes.

Lector sends one request at a time. A host MUST send exactly one response for
every request that has an ID. It MAY send notifications at any time,
including when no request is outstanding.

**Deadlines.** Initialization must complete within five seconds. Every other
call must complete within one second. Notifications do not extend a deadline.

**Transport failures.** End of file, malformed framing, an unexpected
response, a missed deadline, or a message that fails validation (JSON-RPC
version, response ID, result/error exclusivity, event shape, UTF-8, frame
size) is a transport failure. See [section 10](#10-errors-recovery-and-shutdown).

Unknown notification methods, unknown event types, and extra object members
are ignored. They are not failures.

## 4. Initialization

`rpc.discover` MAY be called first. Otherwise the first method MUST be
`initialize`, called exactly once:

```json
{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocol":{"major":2,"minimumMinor":2,"maximumMinor":2},"client":{"name":"lector","version":"0.4.1"},"clientCapabilities":{"speechEvents":true,"progressModes":["marker","utf8ByteOffset"]}}}
```

The host picks a minor version in the offered range and describes itself:

```json
{"jsonrpc":"2.0","id":1,"result":{"protocol":{"major":2,"minor":2},"server":{"name":"lector-tts","version":"0.1.0"},"backend":{"id":"av-foundation","name":"AVFoundation"},"capabilities":{"lifecycle":{"started":{"delivery":"reliable"},"terminal":{"delivery":"reliable","distinguishes":["completed","cancelled","failed"]}},"progress":{"modes":[{"kind":"utf8ByteOffset","granularity":["word"]}]},"controls":{"stop":"confirmed","pauseResume":"restartFromWord"},"settings":{"rate":"readWrite","pitch":"readWrite","volume":"readWrite"},"voices":{"list":true,"current":true,"select":true}}}}
```

Rules:

- A new major version is an incompatible protocol. Minor versions only add
  to it.
- A host SHOULD pick the highest minor version both sides support. If none is
  compatible, it MUST return error `-32001`.
- Names and versions MUST be nonempty.
- `backend`, when present, has a stable nonempty `id` and a nonempty display
  `name`. It describes the session. It does not promise that the backend's
  external service stays available.

### Forward compatibility

Clients MUST accept unknown object members, capability families, capability
values, event types, end reasons, and position kinds. An unknown or missing
capability means "unsupported". An unknown event or position MUST NOT change
playback state. Extensions SHOULD use a vendor-qualified key where a name
clash is plausible.

## 5. Capabilities

Capabilities tell Lector what it can rely on. A host MUST advertise only
behavior it provides for every utterance it accepts.

Only `controls.stop` is required. It must be `confirmed` or `bestEffort`,
because cancelling, replacing, and pausing all depend on it. Everything else
is optional.

### 5.1 Lifecycle

`lifecycle.started.delivery` and `lifecycle.terminal.delivery` are each one
of:

| Value | Meaning |
| --- | --- |
| `reliable` | The event is always sent, exactly as section 8 requires. |
| `bestEffort` | The host may omit it. |
| `unsupported` | Callers cannot depend on it. Unknown values mean this. |

With `reliable` terminal delivery, every accepted utterance MUST produce
exactly one `ended` event, unless the transport dies. This is what allows
Lector to send its next queued utterance.

`terminal.distinguishes` lists the end reasons the host reports accurately.
The conventional values are `completed`, `cancelled`, and `failed`. An
unknown reason still ends the utterance.

### 5.2 Progress

`progress.modes` lists the ways a host can report its position:

- `{"kind":"utf8ByteOffset","granularity":["word"]}`: the position is the
  byte offset of the start of the word being spoken.
- `{"kind":"marker",...}`: the position is an opaque marker ID from a text
  format both sides understand.

A mode is usable only when both sides advertise it. Unknown kinds and
granularities are ignored.

### 5.3 Controls

`controls.stop`:

| Value | Meaning |
| --- | --- |
| `confirmed` | A successful `speech.stop` response proves playback of that utterance will not continue. |
| `bestEffort` | A stop was requested, but the backend cannot confirm it. |

`controls.pauseResume` is `restartFromWord` or `unsupported`.
`restartFromWord` requires word-granularity UTF-8 offsets, and means:

1. `speech.pause` stops audio and returns the start of the word that was
   being spoken.
2. The utterance and its ID remain active, but paused.
3. `speech.resume` starts again at exactly that word.

Resuming may repeat part of a word. It MUST NOT skip the rest of one.

### 5.4 Settings

`settings.rate`, `settings.pitch`, and `settings.volume` are each
`readWrite`, `writeOnly`, or `unsupported`.

- `readWrite` enables both the getter and the setter.
- `writeOnly` enables only the setter. It exists for backends that can apply
  a value but not read it back.
- Every setter returns the value actually in effect.
- Lector calls a setter only for an advertised writable setting. Missing or
  unknown members mean `unsupported`.

**Rate** is normalized. It runs from `0` to `100`, where `0` is the
backend's slowest rate, `50` is its normal rate, and `100` is its fastest.
The host converts in both directions, and native rate units never cross the
connection. If a backend's normal rate sits at one end of its range, the host
uses one smooth mapping across the whole range, so no native value maps to
two normalized values.

**Pitch** and **volume** use the backend's own documented range.

### 5.5 Voices

`voices.list`, `voices.current`, and `voices.select` are independent
booleans, false when missing. Each enables `speech.listVoices`,
`speech.getVoice`, or `speech.setVoice`.

A host MUST NOT invent a voice named `default` when the backend cannot
identify or control its voice. For example, NVDA owns its own voice, so the
built-in NVDA adapter advertises all three as false.

There is no "is speaking" capability or method. Playback state comes from
lifecycle events, never from sampling the backend.

## 6. Data types

**Utterance ID.** A nonempty opaque JSON string of at most 128 UTF-8 bytes,
never a number. Hosts MUST echo it byte for byte and MUST NOT expose a native
identifier instead. Lector never reuses an ID within a host session, so a
late event cannot be mistaken for news about newer speech.

**Event sequence.** A nonnegative integer no greater than
9,007,199,254,740,991. It MUST increase strictly within one utterance. It
need not be consecutive, and means nothing across utterances. Lector ignores
a duplicate or older sequence number.

**Request ID.** A nonnegative integer in the same range. It matches requests
to responses and is unrelated to utterance IDs.

**Position.** All text positions are byte offsets into the UTF-8 encoding of
the exact `speech.speak` text:

```json
{"kind":"utf8ByteOffset","offset":6}
```

`offset` MUST NOT exceed the text's byte length and MUST fall on a character
boundary. An invalid position cannot be used to resume. UTF-16 indexes MUST
NOT cross the connection: a platform that uses UTF-16 converts in the host,
and rejects positions inside a surrogate pair.

## 7. Methods

All parameters are passed by name.

### 7.1 `rpc.discover`

Takes no parameters and returns an OpenRPC document compatible with
[`openrpc.json`](../../crates/lector-tts/openrpc.json). Available before and
after initialization.

### 7.2 `speech.speak`

```json
{"jsonrpc":"2.0","id":2,"method":"speech.speak","params":{"utteranceId":"41:0","text":"first paragraph"}}
{"jsonrpc":"2.0","id":2,"result":{"accepted":true}}
```

- The host MUST reject this call if another utterance is active.
- The host MUST NOT queue utterances itself. Lector sends the next one only
  after reliable evidence that the current one ended.
- A successful response means the host has taken responsibility for the
  utterance. It does not mean playback has started or finished.

### 7.3 `speech.stop`

```json
{"jsonrpc":"2.0","id":3,"method":"speech.stop","params":{"utteranceId":"41:0"}}
{"jsonrpc":"2.0","id":3,"result":{"accepted":true}}
```

- Succeeds harmlessly when nothing is active.
- If the ID is not the active utterance, the host SHOULD return `-32602`.
- A host with reliable lifecycle MUST then send one `ended` event with a
  cancelled or failed reason, unless it already sent the terminal event.

Stop is a primitive. Lector uses it to cancel, to replace, and as its
fallback for pausing. Whether anything is kept for later is Lector's decision.

### 7.4 `speech.pause`

```json
{"jsonrpc":"2.0","id":4,"method":"speech.pause","params":{"utteranceId":"41:0"}}
{"jsonrpc":"2.0","id":4,"result":{"paused":true,"position":{"kind":"utf8ByteOffset","offset":6}}}
```

Only hosts that advertise `pauseResume` implement this.

- `paused: true` MUST include a valid position.
- `paused: false` means there is nothing to resume, and MUST omit the
  position.
- Pausing an already paused utterance succeeds.

If the result is `paused: false`, an error, or an invalid position, Lector
falls back to `speech.stop` and later restarts the whole utterance under a
new ID.

### 7.5 `speech.resume`

```json
{"jsonrpc":"2.0","id":5,"method":"speech.resume","params":{"utteranceId":"41:0"}}
{"jsonrpc":"2.0","id":5,"result":{"accepted":true}}
```

The host resumes the same utterance. For `restartFromWord`, it speaks the
original text again from the position `speech.pause` returned. Later progress
positions remain relative to the complete original text.

The host MUST reject an ID that is not the paused utterance. If resume fails,
Lector stops the utterance and uses the same restart fallback.

### 7.6 Settings

| Method | Parameters | Result | Available when |
| --- | --- | --- | --- |
| `speech.getRate` | none | `{"rate":50}` | `settings.rate` is `readWrite` |
| `speech.setRate` | `{"rate":65}` | `{"rate":65}` | `readWrite` or `writeOnly` |
| `speech.getPitch` | none | `{"pitch":1.0}` | `settings.pitch` is `readWrite` |
| `speech.setPitch` | `{"pitch":1.1}` | `{"pitch":1.1}` | `readWrite` or `writeOnly` |
| `speech.getVolume` | none | `{"volume":1.0}` | `settings.volume` is `readWrite` |
| `speech.setVolume` | `{"volume":0.8}` | `{"volume":0.8}` | `readWrite` or `writeOnly` |

```json
{"jsonrpc":"2.0","id":7,"method":"speech.setRate","params":{"rate":65}}
{"jsonrpc":"2.0","id":7,"result":{"rate":65}}
```

- All values MUST be finite.
- **Rate** MUST be within `0` to `100`. A value outside that range is an
  invalid parameter. It MUST NOT be clamped.
- **Pitch** and **volume** MAY be clamped by the host. The setter returns the
  value in effect.

When Lector replaces a host process, it restores the rate, and restores pitch
and volume only if the user set them. It never imposes one backend's defaults
on another.

### 7.7 Voices

| Method | Parameters | Result | Available when |
| --- | --- | --- | --- |
| `speech.listVoices` | none | `{"voices":[...]}` | `voices.list` |
| `speech.getVoice` | none | `{"voice":{...}}` | `voices.current` |
| `speech.setVoice` | `{"voiceId":"voice-1"}` | `{"voice":{...}}` | `voices.select` |

A voice looks like this:

```json
{"id":"voice-1","name":"Samantha","language":"en-US","gender":"female"}
```

- `id` is an opaque, nonempty string from the backend, used for selection.
- `name` and `language` (in BCP 47 style) are for display.
- `gender` is optional, descriptive, and open-ended.

`speech.getVoice` may return `"voice": null` only when the backend can
actually report that it is using an unnamed default. The three operations are
independent: selecting a voice does not require being able to report the
current one.

## 8. Events

The host reports playback with `speech.event` notifications, which have no
ID:

```json
{"jsonrpc":"2.0","method":"speech.event","params":{"utteranceId":"41:0","sequence":0,"event":{"type":"started"}}}
{"jsonrpc":"2.0","method":"speech.event","params":{"utteranceId":"41:0","sequence":1,"event":{"type":"progress","position":{"kind":"utf8ByteOffset","offset":6}}}}
{"jsonrpc":"2.0","method":"speech.event","params":{"utteranceId":"41:0","sequence":2,"event":{"type":"ended","reason":"completed"}}}
```

| Type | Meaning |
| --- | --- |
| `started` | Playback began. |
| `progress` | Playback reached `position`. |
| `paused` | Playback is paused at `position`. |
| `resumed` | Playback resumed, optionally with a `position`. |
| `ended` | The only terminal event. Has a nonempty `reason` and an optional human-readable `message`. |

A host MUST keep events in order on stdout.

Lector ignores events for an unknown ID, an ID that already ended, an older
host process, or a sequence number that did not increase. Unknown event types
are ignored.

## 9. How Lector queues speech

A host does not implement this section. It describes what Lector does with
the primitives above, so that host authors know what to expect.

### One active utterance

Lector gives the host at most one utterance at a time and keeps the rest in
its own queue. The queue holds at most 256 KiB, counting text, IDs, and
bookkeeping. Past that, the oldest waiting utterances are dropped.

An in-flight utterance is never replayed after a failure, because the host
may already have spoken it.

### Paragraphs

Lector splits each speech request before sending it:

- A single line break becomes a space.
- Two or more line breaks in a row mark a paragraph boundary. CRLF counts as
  one line break.
- Each nonempty paragraph is sent as its own utterance, with its own opaque
  ID. Hosts MUST NOT read meaning into the format of IDs.

After a paragraph's `ended` event, Lector waits for the user's configured
paragraph pause, 100 ms by default, before sending the next. The pause is
Lector's own. It is not a host timer or a protocol field.

Without reliable terminal events, Lector cannot know when a paragraph ends.
It then joins the paragraphs with spaces and sends one utterance.

### Speaking

| Request | While speaking | While paused | When idle |
| --- | --- | --- | --- |
| Speak, not interrupting | Added to the queue | Replaces everything kept | Starts |
| Speak, interrupting | Replaces everything | Replaces everything | Starts |

The silent gap between paragraphs counts as speaking.

### Pausing and resuming

Typing and other ordinary input pause speech. The user can also pause,
resume, toggle, and cancel explicitly. All of these do nothing when idle.

- **With `restartFromWord`:** pause keeps the utterance, and resume restarts
  at the interrupted word.
- **Without it:** pause stops the utterance and keeps its full text. Once a
  confirmed stop, or a reliable `ended` event, proves the old audio is gone,
  resume sends the text again from the beginning under a new ID. A resume
  requested while a best-effort stop is still pending is remembered.
- **Between paragraphs:** pause freezes the queue without contacting the
  host. Resume starts the next paragraph at once.
- A missing or invalid pause position is treated as "without it" above.

Either way, the queue of waiting speech is kept.

**Cancel** discards the active or paused utterance and everything queued,
then stops the host.

### Without reliable terminal events

Lector cannot safely send a second utterance to a version 2 host that lacks
reliable terminal events, so it refuses to queue one. Basic speech still
works.

## 10. Errors, recovery, and shutdown

| Code | Meaning |
| ---: | --- |
| `-32700` | Parse error |
| `-32600` | Invalid request or state transition |
| `-32601` | Method or capability not supported |
| `-32602` | Invalid parameters or utterance ID |
| `-32603` | Internal speech-backend failure |
| `-32001` | No compatible protocol version |

A well-formed error response means the operation failed. It does not mean the
host crashed.

A [transport failure](#3-transport) does. Lector kills and reaps that host
process. Only speech that was never sent survives into the replacement.

**Restart policy.**

- At startup, Lector tries twice.
- While running, Lector allows one automatic restart per rolling 30 seconds.
  A second failure inside that window is fatal.
- If the restarted host fails to start, that is fatal too.
- Replacing a host on purpose, with `lector.api.set_speech`, is not a crash.
  The old host stays selected until the new one has initialized and had its
  settings restored.

**Backends that come and go.** A backend's external service may disappear
without the transport failing. The built-in NVDA backend stays alive when
NVDA stops or is not running. Requests made meanwhile are consumed, not
queued for later, and an accepted utterance is completed with a failed
`ended` event. Later requests try NVDA again, so speech returns when NVDA
does.

**Shutdown.** On normal exit, Lector terminates and reaps the host. The
built-in host also watches its parent process. Custom hosts SHOULD exit on
stdin end of file, or use an equivalent guard around blocking platform calls.

## 11. Older protocol versions

### 2.0 and 2.1

Earlier 2.x versions used the backend's native units for rate. Lector's rate
option is normalized, so Lector offers 2.2 as its minimum minor version. A
newer host that serves an older client keeps using that version's native
rate.

Pitch, volume, and `speech.getRate` were added in 2.1. When 2.0 is selected,
a rate of `readWrite` means `writeOnly`, and pitch and volume are
unsupported.

### Version 1 and unversioned hosts

Lector offers version 2 first.

- If `initialize` returns `-32001`, Lector retries with version 1.0 and its
  method names: `speak`, `stop`, and `set_rate`.
- If `initialize` returns `-32601`, Lector treats the process as an
  unversioned legacy host.

Legacy hosts have no lifecycle or progress events and keep their own internal
queue. As a result:

- Lector cannot resume from a word. It can stop, but without evidence that
  the stop finished it never resends, so resume may be unavailable.
- Multi-paragraph requests are sent as one utterance.
- `set_rate` receives the normalized `0` to `100` value. The legacy adapter
  must convert it.

New hosts MUST implement version 2.

## 12. Implementation checklist

A conforming version 2 host:

1. Reserves stdin and stdout for NDJSON JSON-RPC, keeps frames under 1 MiB,
   and flushes each one.
2. Implements `initialize` with version ranges, `rpc.discover`, and explicit
   capabilities. It omits, or marks unsupported, anything it cannot
   guarantee.
3. Accepts at most one active utterance and echoes its ID unchanged.
4. Sends correlated, strictly sequenced events exactly as advertised.
5. Converts native positions to markers or valid UTF-8 byte offsets.
6. Implements pause and resume only if it can restart at the interrupted
   word. Otherwise it advertises them as unsupported.
7. Converts rate to and from the normalized `0` to `100` range.
8. Never invents a voice it cannot control.
9. Responds within the deadlines, exits on end of file, and cleans up its
   child processes.
