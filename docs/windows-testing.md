# Windows testing

`heya-windows-x64-installer` is the ordinary MIT build. It includes the Rust
audio engine and uses CPAL's Windows WASAPI backend. It deliberately has no
load-time MPV dependency, so it must launch on a clean Windows 10/11 machine
and retain browser video playback when MPV is unavailable. Its local settings
window can install the pinned MPV runtime into per-user app data after explicit
approval; MPV is not bundled in the application or NSIS installer.

Windows processed audio supports:

- Output-device enumeration and selection.
- Sample-rate conversion to the active WASAPI shared-mode format.
- ReplayGain, EQ, pre/post gain, limiter, crossfeed, and crossfade.
- Gapless playback, queue resume, seek, waveform, and FFT visualizers.
- Real-time/high-priority audio callback scheduling through CPAL.

Bit-perfect playback is not advertised on Windows yet. CPAL's current WASAPI
output is shared-mode and may invoke the Windows mixer/converter. A future
WASAPI exclusive adapter must prove exact device format negotiation before the
setting is enabled.

## Tester checklist

Record the Windows version, CPU architecture, GPU, driver version, and audio
devices used. Then verify:

1. Heya opens and can connect to both a normal HTTPS server and a Tailscale
   server.
2. `Ctrl+,` opens settings and reports the expected MPV/audio capabilities.
3. The Output tab lists the system default, HDMI, USB, Bluetooth, and other
   available endpoints without duplicates or blank labels.
4. Selecting an endpoint routes the next playback session to it and the choice
   survives reopening settings.
5. Music starts, pauses, seeks, resumes after reload, advances, and stops.
6. EQ, ReplayGain (including album mode), crossfeed, limiter, gapless playback,
   timed crossfade, smart crossfade, and same-album suppression each work without
   stutter.
7. The waveform spans the complete track and its visible features stay aligned
   with playback; FFT visualizers continue updating under native playback.
8. Changing or disconnecting the active Windows output produces a visible
   playback error instead of silent, apparently-playing audio. Starting again
   after selecting a valid device recovers.
9. A clean install reports MPV unavailable, uses browser video, and offers the
   verified MPV installation action only in local settings.
10. Approving installation downloads, verifies, and activates MPV without
    restarting HeyaClient. Reopening settings reports MPV available and native
    video opens in a separate player window.
11. MPV play/pause, seek, volume, mute, audio tracks, subtitles, fullscreen,
    resizing, window close, and replay all work.
12. The information panel identifies MPV and reports the actual hardware
    decoder (`d3d11va`, `d3d11va-copy`, or the fallback in use).
13. Closing MPV does not mark unfinished media complete. Closing Heya,
    switching servers, logging out, and reloading dispose playback cleanly.
14. The Windows media flyout shows title, artist, album, artwork, duration, and
    the correct playing state for both browser and native Rust audio output.
15. Hardware play/pause, previous, next, and seek commands operate the Heya
    queue while HeyaClient is focused and while another app is focused.
16. Enabling track-change notifications in local settings shows one
    notification after a real background track change, but none while the
    HeyaClient window is focused or when metadata refreshes in place.

Debug logs should be included with any failure report, but playback
grants, media URLs, cookies, and account credentials must be removed before
sharing them.
