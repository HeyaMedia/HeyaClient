# Native playback foundation

HeyaClient owns native rendering, renderer lifecycle, and runtime diagnostics.
The remote Heya UI owns playback intent, controls, queue/up-next behavior,
progress reporting, source selection, and presentation. The selected Heya
origin receives a semantic playback bridge; it never receives a raw MPV
command, property setter, property subscription, arbitrary header, local path,
shell operation, or user bearer token.

## Bridge v1

The public object is `window.__HEYA_NATIVE_PLAYBACK__`. It is frozen and is
installed asynchronously only after the current main-frame URL, request
`Origin`, and saved server origin all match. `heya_client=1` is not consulted.

The methods are:

- `getPlaybackCapabilities()`
- `loadPlayback(request)`
- `sendPlaybackCommand(command)`
- `subscribePlaybackState(listener)`
- `subscribePlaybackDiagnostics(listener)`
- `disposePlayback({ rendererSessionId })`

The ready event is `heya:native-playback:ready-v1`. Rust publishes normalized
state and diagnostics through internal DOM events named
`heya:native-playback:state-v1` and
`heya:native-playback:diagnostics-v1`; the public subscription methods filter
them to the owning page instance. A page can spoof DOM events to itself, but
doing so does not cause a native operation.

The exact TypeScript contract is in
[`native-playback-bridge.d.ts`](native-playback-bridge.d.ts). Production
`loadPlayback` accepts only a same-origin Heya native-media URL, one opaque
64-character playback grant, and an optional start position.

The local `Cmd/Ctrl+,` settings window shows the result of HeyaClient's MPV
capability probe. `native_playback_enabled` defaults to `true` and is stored in
the native app settings. Turning it off makes the bridge advertise no native
capabilities and reject new native loads with `backend_unavailable`; an
existing playback session is allowed to finish. The selected remote Heya
origin cannot read or change this local preference through generic Tauri IPC.

Transport uses one narrowly permissioned Tauri command. Its remote capability
is generated for the exact saved origin and the main window only. Every
operation then rechecks the WebView label, live main-frame URL, and saved Heya
origin in Rust. Media URLs must use HTTP(S), match that origin, contain no
credentials or fragment, and live below `/api/playback/native/media/`.

### Tauri framework boundary

Tauri 2.11 injects its non-configurable `window.__TAURI_INTERNALS__` bootstrap
into every Tauri-managed WebView. There is currently no supported per-WebView
switch to suppress it. The selected remote origin receives only the two
semantic Heya bridge permissions (native audio and native playback); generic
Tauri commands remain denied. The frozen public bridge objects use the
framework bootstrap internally, but they do not expose raw `invoke`, arbitrary
command names, or arbitrary native payloads. HeyaClient accepts the visible
internals object as part of Tauri's framework runtime and retains independent
origin and schema validation in Rust.

## Renderer manager

`NativePlaybackManager` owns at most one active renderer. Starting a
replacement first synchronously stops and disposes the old engine. A dedicated
worker owns each engine and serializes commands. `commandId` results are cached
within the active session for retry deduplication, while `commandSequence`
records acceptance order.

State and diagnostics have independent monotonic revisions. Position events
are coalesced to roughly 4 Hz and diagnostics to roughly 1 Hz. Structural,
track, format, error, and termination changes publish immediately.

Termination reasons are `ended`, `stopped`, `window_closed`, `disposed`,
`failed`, `native_crashed`, `logged_out`, `server_switched`, and `app_quit`.
Only a natural MPV EOF becomes `ended`.

`PlaybackEngine` and `PlaybackEngineFactory` isolate the manager and bridge
from libmpv. Unit tests use a fake engine. The current platform adapter uses
the direct `libmpv2` binding rather than either third-party Tauri MPV plugin.

## Diagnostics boundary

The MPV adapter observes only an allowlist of scalar properties. It builds the
normalized Heya diagnostics schema inside Rust and never forwards raw MPV
events. It does not query `perf-info`, paths, filenames, URLs, HTTP headers,
cookies, grants, or arbitrary metadata. String labels pass an additional
sanitizer before publication.

Track-list nodes stay native. Heya receives generated IDs such as `audio:1`
and `subtitle:2`; raw MPV `aid` and `sid` values remain in an in-memory map.
Variant selection deliberately reports that a replacement server descriptor
is required instead of pretending an HLS level is an MPV property.

## Native development harness

Debug builds with `--features native-mpv` add native-only Server menu items.
The harness checks `HEYA_MPV_TEST_MEDIA` first, then looks for the largest MKV
directly inside the adjacent Heya Avatar development directory, and otherwise
uses a synthetic lavfi source. The selected path never enters the bridge,
events, or logs.

Use `bun run dev:native` for the Heya bridge plus MPV backend, or
`bun run dev` to exercise the intentionally unavailable/fallback path.

The first surface is a separate MPV-owned native window. Heya HTML controls are
not layered over it.

## Development-only libmpv spike

HeyaClient is MIT-licensed. The current `native-mpv` feature links the
developer's local libmpv and is not a public distribution configuration. No
release workflow bundles the development renderer or publishes its dependency
graph.

The current macOS development staging helper wraps MPV's official
`TOOLS/dylib_unhell.py`:

```sh
bun run native:stage:macos -- \
  src-tauri/target/debug/bundle/macos/Heya.app \
  --mpv-source /path/to/pinned/mpv \
  --adhoc-sign

bun run native:verify:macos -- \
  src-tauri/target/debug/bundle/macos/Heya.app
```

The helper exists only to test native playback on development machines. Public
MIT builds use the browser backend until the optional, user-initiated runtime
installation described below is implemented; they must not carry the
development libmpv graph.

The approved optional runtime-install direction and renderer handoff are in
[`runtime-mpv-installation.md`](runtime-mpv-installation.md).

When a staged bundle contains MoltenVK, HeyaClient pins `VK_DRIVER_FILES` to
the bundled manifest before Tauri starts so a developer's global Vulkan driver
is not loaded alongside it.

## Authentication and networking

Heya's normal bearer token never enters HeyaClient or MPV. Heya issues an
opaque grant bound to the login session and one media subtree. The native
adapter constructs exactly one fixed header from it:

`X-Heya-Playback-Grant: <opaque grant>`

The UI cannot select the header name or provide MPV options. The allowlisted
Heya media routes cover direct byte ranges, manifests, segments, subtitles,
seeks, and quality replacements and are contractually non-redirecting. Every
media request revalidates the originating Heya login session, so logout or
session revocation invalidates the grant without exposing the bearer token.
