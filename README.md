# Lightair Core

The crates every Lightair app is built on: the desktop app
([Lightair-Project/lightair](https://github.com/Lightair-Project/lightair)), Lightair Live and Lightair
Editor all use them, so a fix here reaches all three.

| Crate | Does |
|---|---|
| `lightair-model` | The project model: scenes, layers, inputs, formats; commands, undo, validation. No I/O |
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
lightair-model = { path = "../lightair-core/crates/lightair-model" }
```

## Working on lightair-core and an app together

The apps (`lightair`, `lightair-live`, `lightair-editor`) sit beside this repo in one folder and
pin lightair-core to a commit on GitHub. To change both at once, run these here:

```sh
cargo xtask dev on       # every app builds against this checkout instead of its pin
cargo xtask dev status   # who's linked, pinned where, what's uncommitted
cargo xtask ship         # check and push lightair-core, then pin, check and commit each linked app
cargo xtask dev off      # back to the pins without shipping
```

While linked, only each app's `Cargo.lock` changes; don't commit it (a pre-commit guard
refuses), since `ship` pins and commits it. `ship --push` pushes the apps too, `--only
desktop,live,editor` picks apps, and `--dry-run` only reports. The project's working rules are in
`../CLAUDE.md` (beside the repos, not in git).

## Checking

There's no hosted CI. Before a push:

```sh
cargo xtask check
```

## Licence

Copyright © 2026 Bregas S. Wicaksono (MoonGoblin).

Dual-licensed: **GPL-3.0-or-later** (`LICENSE`) for everyone, or a **commercial licence** from
the copyright holder, which Lightair's mobile apps use. Details in `LICENSING.md`; contributions
need the contributor licence agreement (`CONTRIBUTING.md`). The licence audit is
`docs/licence-audit.md`. The font used by tests is Inter, under the SIL Open Font License
(`crates/lightair-fonts/tests/data/Inter-OFL.txt`).
