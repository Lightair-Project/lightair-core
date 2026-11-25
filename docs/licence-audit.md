# Licence audit

Done 20 November 2026, at lightair-core commit `9d3635f` and later, before choosing how
lightair-core is licensed to Lightair Live and Lightair Editor (proprietary App Store apps). The
question: is there anything in lightair-core that its owner, Bregas S. Wicaksono (MoonGoblin),
didn't write and so couldn't license on other terms?

**Answer: no third-party GPL code was found.** Everything in the repository is the owner's own
work, except a few permissively licensed files that carry their notices (§3).

## 1. Authorship

| Check | Result |
|---|---|
| Commit authors | One person: the owner, committing as MoonGoblin under the GitHub noreply address (239 commits on 25 November 2026; the history was rewritten that day, and three early commits that had the owner's personal address now carry the noreply one too) |
| Outside contributions (pull requests, patches, co-authors) | None |
| Files carrying someone else's copyright or licence notice | Only `crates/lightair-widgets/assets/jquery-3.7.1.min.js` (MIT, §3) |

## 2. Code that touches other projects

| What | Where | Finding |
|---|---|---|
| Effect shaders (`.effect`, 22 files + `common.inc`) | `crates/lightair-render/fx/` | Written in this project: first added in the desktop repo in `ebe8780` (12 November 2026, "lightair_fx: data-driven shader effects"), not taken from Dewy or OBS. Compared line by line with all 52 `.effect` files in OBS Studio's source: the only shared lines are the effect system's own API boilerplate, the technique line `vertex_shader = VSDefault(v_in);` and the two-line default vertex shader in `common.inc`. That's how a shader plugs into libobs, not creative expression; the shader bodies are original |
| WGSL ports of those effects | `crates/lightair-render/src/fx.wgsl`, `fx_prelude.wgsl` | Ports of the files above, so the same origin |
| obs-websocket v5 compatibility | `crates/lightair-remote/src/obs.rs` | An implementation of the documented protocol (message names, close codes, the auth handshake) in Lightair's own structure, mapping to Lightair's methods. No obs-websocket source is in the repository and none was copied. Reimplementing a documented protocol isn't copying its code |
| OBS scene collection import | `crates/lightair-store/src/import_obs.rs`, `studio_take_scene.rs` | Reads OBS's JSON format into Lightair's model; original translation code |
| StreamElements and Streamlabs widget compatibility | `crates/lightair-widgets/assets/se-shim.js`, `sl-shim.js` | Original scripts that recreate those platforms' widget APIs so imported widgets run; no platform code copied. Platform names are used only to say what's compatible |
| Built-in widgets (19 pages and their fields) | `crates/lightair-widgets/assets/widgets/` | Original |
| Speech to text | `crates/lightair-render/src/speech.rs` | Runs whisper.cpp (MIT) as a separate helper program; nothing linked or copied |
| OBS test fixture | `crates/lightair-store/tests/fixtures/obs-collection.json` | Made for the tests |
| SRTLA bonding | `crates/lightair-feed/src/srtla.rs` | Written from the SRTLA protocol (how a sender registers its links with a receiver and spreads packets over them), to work with existing receivers; no receiver code is in the repository or was copied |
| RTMP, RTMPS, SRT and MPEG-TS out | `crates/lightair-feed/src/` | Written from the protocols' specifications; SRT itself comes from the `srt-tokio` crate (Apache-2.0) |
| ffmpeg in the feed's tests | `crates/lightair-feed/tests/` | ffmpeg is run as a separate program (an RTMP server and a decoder to check what was sent), when it's installed; nothing linked or copied |
| Text and timer styles from the desktop's settings | `crates/lightair-overlay/src/text.rs` | Reads the settings the desktop saves for text sources (libobs's `text_ft2_source_v2` fields and colour format) so a show draws the same everywhere; reading a file format isn't copying code |

## 3. Third-party files in the repository

| File | Licence | Obligation |
|---|---|---|
| `crates/lightair-widgets/assets/jquery-3.7.1.min.js` | MIT | Keep its header; list it in the apps' licences screen. Source and hash in `assets/SOURCE.md` |
| `crates/lightair-fonts/tests/data/Inter-Variable.ttf` | SIL OFL 1.1 (`Inter-OFL.txt` beside it) | Test data only; keep the licence with it |
| `crates/lightair-fonts/catalog/fonts-catalog.json` | Facts about Google Fonts' families (names, designers, files, hashes) from `google/fonts` | Factual data; the fonts themselves are downloaded, each with its own licence (docs/fonts.md §2) |

## 4. Dependencies

345 third-party crates in the dependency tree (`cargo metadata`, all features and platforms):

| Licence | Crates |
|---|---|
| MIT and/or Apache-2.0 (in their various spellings) | 291 |
| Zlib (alone or as one choice) | 21 |
| BSD-3-Clause | 7 |
| ISC | 3 |
| Unlicense OR MIT | 3 |
| Others below | 3 |

Worth noting:

- `self_cell` (Apache-2.0 OR GPL-2.0-only) and `r-efi` (MIT OR Apache-2.0 OR LGPL-2.1-or-later):
  each offers a permissive choice, which is the one used.
- `option-ext` (MPL-2.0): file-level copyleft, compatible with proprietary apps and the App Store;
  list it, with a pointer to its source.
- `webpki-root-certs` (CDLA-Permissive-2.0): permissive; list it.

No dependency is GPL-, LGPL- or AGPL-only. Each app's own `cargo deny` enforces this on its
whole tree (lightair-live docs/11 §1).

## 5. Notes

- The desktop app's Lightair Effect Stack module (from Dewy, GPL-2.0-or-later, same author) isn't
  in lightair-core; only the effect shaders above are, and they were written here.
- Code written with AI assistance is the owner's to license, but how much of it carries copyright
  depends on the human authorship in it. This doesn't create a licensing risk for the apps; it
  only means some code may be less protectable than hand-written code.
- This is an engineering audit, not legal advice. Before a commercial launch, have a lawyer look
  at the licensing statement, the contributor terms and, if a company publishes the apps, the
  licence from the owner to that company.

## 6. Keeping it clean

- **Decision (20 November 2026): dual licence**, GPL-3.0-or-later or the copyright holder's
  commercial terms (`LICENSING.md`). Outside contributions only under the contributor licence
  agreement (`CONTRIBUTING.md`, `docs/CLA.md`), so every line stays licensable both ways.
- Before adding any file not written here, record its source and licence (as `SOURCE.md` does
  for jQuery).
- Re-run the dependency scan when dependencies change: each app's `cargo deny`, and for
  lightair-core itself, the script behind §4 (`cargo metadata` and a licence filter).
