# Native audio

HeyaClient owns audio decoding, output, DSP, and renderer lifecycle. Heya owns
the queue, playback intent, UI, server progress, and scoped media grants. The
remote page receives a narrow semantic bridge; it never receives generic
Tauri IPC, local paths, arbitrary URLs or headers, output-device handles, or a
Heya bearer token.

## Bridge

The selected saved Heya origin receives the frozen
`window.__HEYA_NATIVE_AUDIO__` protocol-v1 object. Every operation validates
the live main-frame origin again. Transport uses one narrowly permissioned
Tauri command whose remote capability is generated for that exact origin and
the main window only. It does not expose arbitrary Tauri commands. The ready
event is `heya:native-audio:ready-v1`; normalized state and visualizer
snapshots are delivered through the bridge's subscription methods.

The exact public shape is documented in
[`native-audio-bridge.d.ts`](native-audio-bridge.d.ts). Production loads accept
only a same-origin `/api/playback/native/media/` URL and the fixed opaque
`X-Heya-Playback-Grant` credential. Redirects are rejected. Grants and URLs are
redacted from Rust debug output and are never sent back to the page.

## Processed mode

The Rust engine uses two decoded PCM decks and a CPAL output stream. It supports
gapless transitions, timed and smart crossfade, album-aware suppression,
ReplayGain, pre/post gain, a 10-band EQ, headphone crossfeed, a limiter, and
software volume. The EQ card's equalizer/crossfeed order is preserved. Shared
output supports CPAL's integer and floating-point device formats, with browser
playback retained as the startup fallback.

HeyaClient enumerates CPAL output devices and exposes only normalized stable
IDs, display labels, and the system-default flag through the audio bridge. A
specific processed-mode output can be selected from Heya's Output tab; the
choice is persisted in `app-settings.json`. Passing `null` selects “follow the
system default.” An active native session is replaced at its current position
when the output changes because a CPAL stream is bound to its device at open.

EQ profiles remain Heya-owned UI preferences and are keyed to those stable
output IDs. They contain only EQ, pre/post gain, and crossfeed. A device with
no saved profile receives Flat EQ with crossfeed disabled; ReplayGain,
crossfade, limiter, and DSP ordering remain global playback settings.

Post-DSP PCM and FFT snapshots drive Heya's mini meter, spectrum, scope, VU,
and starfield. The server-analyzed waveform remains backend-neutral. Milkdrop
falls back to Spectrum under native playback because butterchurn requires a
real WebAudio `AnalyserNode` rather than copied PCM samples.

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

## Bit-perfect mode

The first exclusive adapter is macOS CoreAudio. It acquires hog mode on the
default device, switches to the source sample rate, opens an exact float output
configuration, and releases hog mode when the session ends. It accepts known
lossless sources up to 24-bit PCM and requires the decoded sample rate and
channel count to match the exclusive output. A mismatch stops playback rather
than resampling while claiming bit-perfect output.

The initial macOS exclusive helper can acquire only the OS default CoreAudio
device. Heya therefore locks output selection while bit-perfect is enabled. A
specific device can still be used by making it the macOS system default first;
arbitrary exclusive-device selection is separate future platform work.

Bit-perfect mode forces gapless-only playback and bypasses ReplayGain, EQ,
pre/post gain, crossfeed, limiter, software volume/mute, crossfade, resampling,
and visualizer capture. Heya keeps processed settings saved and restores them
when bit-perfect mode is turned off. If exclusive startup fails, the preference
is restored to processed mode and playback safely resumes there.

The local Cmd/Ctrl+, settings window and Heya's EQ card both control the same
native preference. Linux and Windows currently advertise processed playback
only; their exclusive adapters remain future platform work.
