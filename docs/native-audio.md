# Native audio

HeyaClient owns audio decoding, output, DSP, and renderer lifecycle. Heya owns
the queue, playback intent, UI, server progress, and scoped media grants. The
remote page receives a narrow semantic bridge; it never receives generic
Tauri IPC, local paths, arbitrary URLs or headers, output-device handles, or a
Heya bearer token.

## Bridge

The selected saved Heya origin receives the frozen
`window.__HEYA_NATIVE_AUDIO__` protocol-v2 object. Every operation validates
the live main-frame origin again. Transport uses one narrowly permissioned
Tauri command whose remote capability is generated for that exact origin and
the main window only. It does not expose arbitrary Tauri commands. The ready
event is `heya:native-audio:ready-v2`; normalized state and visualizer
snapshots are delivered through the bridge's subscription methods.

Protocol v2 also requires `getAudioState()`. Heya samples that path as a
liveness check while music is active and projects the clock between samples.
The response reads the Rust callback's PCM-frame atomics directly, so a
dropped engine event or WebView event cannot leave the visible playhead frozen
while audio continues. Push events are reserved for lifecycle changes rather
than relaying a redundant position snapshot four times per second.

The exact public shape is documented in
[`native-audio-bridge.d.ts`](native-audio-bridge.d.ts). Production loads accept
only a same-origin `/api/playback/native/media/` URL and the fixed opaque
`X-Heya-Playback-Grant` credential. Redirects are rejected. Grants and URLs are
redacted from Rust debug output and are never sent back to the page.

## Audio pipeline

The Rust engine uses two decoded PCM decks and a CPAL output stream. It supports
gapless transitions, timed and smart crossfade, mandatory album-continuity suppression,
ReplayGain, pre/post gain, a 10-band EQ, headphone crossfeed, a limiter, and
software volume. The EQ card's equalizer/crossfeed order is preserved. Shared
output supports CPAL's integer and floating-point device formats, with browser
playback retained as the startup fallback.

Smart crossfade consumes the same server-analyzed outro, natural-fade, and
silence boundaries as WebAudio. A detected natural fade uses a linear outgoing
curve; other transitions use equal-power curves, with MixRamp and the configured
timed duration retained as fallbacks. Adjacent tracks from the same album and
repeat-one loops are always gapless; this is a queue invariant rather than a
user preference. ReplayGain policy remains
Heya-owned: Heya resolves track versus album loudness and true-peak headroom,
then passes the resulting per-track gain to both native decks. If loudness or
boundary analysis arrives after loading, a narrow track-analysis command updates
the matching deck and re-arms its transition without restarting playback.

HeyaClient enumerates CPAL output devices and exposes only normalized stable
IDs, display labels, and the system-default flag through the audio bridge. A
specific output can be selected from Heya's Output tab; the
choice is persisted in `app-settings.json`. Passing `null` selects “follow the
system default.” An active native session is replaced at its current position
when the output changes because a CPAL stream is bound to its device at open.

EQ profiles remain Heya-owned UI preferences and are keyed to those stable
output IDs. They contain only EQ, pre/post gain, and crossfeed. A device with
no saved profile receives Flat EQ with crossfeed disabled; ReplayGain,
crossfade, limiter, and DSP ordering remain global playback settings.

Post-DSP PCM and FFT snapshots at 30 Hz drive Heya's live spectrum, scope, VU,
and starfield views. Static icons do not enable the analyser bridge. The
server-analyzed waveform remains backend-neutral. Milkdrop
uses butterchurn's explicit audio-level input under native playback rather
than connecting a WebAudio `AnalyserNode`.

### Buffering and real-time safety

Playback waits for roughly three seconds of decoded PCM before the first
sample. Decoder channels are bounded and the callback consumes at most one
batch per deck per device deadline, so a fast decoder cannot make one callback
copy an entire track. Track-sized PCM capacity is allocated on the control
thread and retired on a reclaimer thread. Visualizer callback buffers are drawn
from a preallocated pool rather than allocating at 60 Hz.

If network delivery still runs dry, HeyaClient emits a distinct underrun log,
enters buffering, and waits for two seconds of decoded headroom (or confirmed
end-of-file) before resuming. That hysteresis avoids rapid play/silence cycling
on a marginal connection.

## Output policy

Protocol v2 deliberately has one reliable shared-output pipeline. The former
experimental bit-perfect/exclusive branch was removed: it rebuilt the renderer,
changed system device state, disabled normal controls and DSP, and did not have
enough platform coverage to justify a second lifecycle. Source and output
formats remain visible in diagnostics, and resampling is reported explicitly.
