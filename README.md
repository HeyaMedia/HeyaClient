# HeyaClient

The native desktop client for [Heya](https://github.com/HeyaMedia/Heya).

It wraps your own Heya server in a Tauri app and adds the native bits that are
hard to do well in a browser: Rust-powered music playback, optional MPV video,
native settings, remembered windows, and automatic updates.

It is still young, but it already plays music and video rather nicely.

## Running it

Install the normal [Tauri prerequisites](https://v2.tauri.app/start/prerequisites/),
then:

```sh
bun install
bun run dev
```

For development with the native MPV backend enabled:

```sh
bun run dev:native
```

MPV is optional. On macOS, install it with `brew install mpv`; Heya discovers
the system library at runtime and never bundles it. If it is unavailable,
video falls back to Heya's browser player. The native Rust audio engine is part
of the regular client build.

## Releases

Tagged releases build macOS ARM64 and Windows x64 installers. HeyaClient checks
GitHub Releases for signed updates and asks before installing one.

Release details live in [docs/releases.md](docs/releases.md).

## License

MIT. See [LICENSE](LICENSE).
