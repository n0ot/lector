# lector-tts

This crate combines Lector's cross-platform TTS library with its standalone
stdio speech host. The library remains available as `tts`, while the
`lector-tts` binary speaks the versioned protocol documented in
[`../../docs/speech-driver-protocol.md`](../../docs/speech-driver-protocol.md).
The implementation is shared with `lector tts`, so installing the main Lector
binary is sufficient when the terminal and speech host run on the same
machine.

```shell
lector-tts --list-backends
lector-tts --backend av-foundation --list-voices
lector-tts --backend av-foundation --voice VOICE_ID
```

With no listing option, stdin and stdout are reserved for bounded NDJSON
JSON-RPC. `--backend auto` (the default) selects the first currently available
engine. Stable backend IDs, selected backend metadata, rate support, and the
three voice operations are reported explicitly rather than inferred.
Protocol 2.2 exposes rate as `0` (slowest) through `100` (fastest); the host
converts that normalized value to each selected backend's native units.

The host is portable independently of the full Lector application. It may be
launched locally, across an SSH stdio bridge, or through another transport
chosen by the user. The bridge owns authentication and reconnection policy.

For NVDA, the controller-client DLL must be loadable. NVDA itself may start or
stop while the host remains alive: speech and interruption requests received
while it is absent are dropped rather than queued for replay. Voice and rate
controls are not advertised for NVDA, and this protocol version does not
expose `is_speaking`.

The underlying library is based on
[`tts-rs`](https://github.com/ndarilek/tts-rs); exact provenance is recorded in
[`UPSTREAM.md`](UPSTREAM.md).

## Library backends

This library provides a high-level Text-To-Speech (TTS) interface supporting various backends. Currently supported backends are:

* Windows
  * NVDA via the [NVDA Controller Client](https://github.com/nvaccess/nvda/tree/master/extras/controllerClient) (requires shipping `nvdaControllerClient.dll` with your application)
  * Screen readers/SAPI via Tolk (requires `tolk` Cargo feature)
  * WinRT
* Linux via [Speech Dispatcher](https://freebsoft.org/speechd)
* macOS/iOS/tvOS/watchOS/visionOS via AVFoundation (macOS 10.14 and above)
* Android
* WebAssembly

## Android

Plug-and-play like the other platforms — no Java sources, Gradle plugin, or manifest entries in your
app — given two things:

* **`minSdkVersion` 26 or above.** The `UtteranceProgressListener` subclass Android callbacks need
  ships as an embedded dex, loaded with `InMemoryDexClassLoader`.
* **A `Context` supplied before the first `Tts`**, one of two ways. [`ndk-context`] is consulted by
  default; [`android-activity`] publishes there for you, including by way of `winit` or a game
  engine. Or call `tts::android::set_context` with a `JavaVM` and `Context` of your choosing — it
  takes precedence over `ndk-context` and is the right entry point when the process outlives its
  `Activity` or never has one, such as speech from a foreground service: `ndk-context`'s slot is
  typically owned by the `Activity`'s glue and released when it's destroyed, while `set_context`
  holds its own reference for as long as backends need it. Either way the supplied context is traded
  for its application context, so no `Activity` is pinned.

Nothing here waits for the engine. Android reports it ready on the app's Java main thread, so a
backend that blocked for that would deadlock any app whose main thread is itself waiting on the
caller — which is every `NativeActivity` app, since [`android-activity`] holds the main thread until
the event loop acknowledges each lifecycle callback. `Tts::default()` therefore returns as soon as
the engine has been *asked* to connect, and anything spoken before it answers is queued and replayed
in order. The one exception is `synthesize`, which returns audio and so has to wait for it; call
that off your event-loop thread.

See _examples/android\_hello\_world.rs_, built with [`cargo-apk`]:

```shell
cargo apk run --example android_hello_world
adb logcat -s tts
```

Editing _android/Bridge.java_ needs a JDK and `ANDROID_HOME`; _build.rs_ rebuilds the checked-in dex
from it.

[`ndk-context`]: https://crates.io/crates/ndk-context
[`android-activity`]: https://crates.io/crates/android-activity
[`cargo-apk`]: https://crates.io/crates/cargo-apk
