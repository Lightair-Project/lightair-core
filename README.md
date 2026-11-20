# Lightair Core

The crates every Lightair app is built on: the desktop app
([MoonGoblinDev/lightair](https://github.com/MoonGoblinDev/lightair)), Lightair Live and Lightair
Editor all use them, so a fix here reaches all three.

| Crate | Does |
|---|---|
| `lightair-core` | The project model: scenes, layers, inputs, formats; commands, undo, validation. No I/O |
| `lightair-store` | Saving and loading projects: the journal, migrations, imports |
| `lightair-audio` | The audio graph and its DSP (Voice Clarity, loudness, noise suppression) |
| `lightair-render` | The offline renderer and the Editor's model: sequences, takes, effects, export |
| `lightair-fonts` | The Google Fonts catalogue, font references, a verified cache, the renderer's font set (`docs/fonts.md`) |
| `lightair-live` | Platform accounts, chat and events |
| `lightair-widgets` | On-stream widgets: chat box, alerts, goals… |
| `lightair-automation` | Triggers and actions; Streamer.bot import |
| `lightair-remote` | The remote-control API: JSON-RPC, obs-websocket, MCP |
| `lightair-platform` | Paths, credentials and OS integration, per platform |

None of them depend on libobs, GPUI or Chromium; those stay in the desktop app.

## Using it

The apps depend on these crates by git at a pinned commit. Side by side in one folder, an app
can build against a local checkout instead:

```toml
lightair-core = { path = "../lightair-core/crates/lightair-core" }
```

## Checking

There's no hosted CI. Before a push:

```sh
cargo xtask check
```

## Licence

GPL-3.0-or-later (`LICENSE`). The fonts used by tests are Inter, under the SIL Open Font License
(`crates/lightair-fonts/tests/data/Inter-OFL.txt`).
