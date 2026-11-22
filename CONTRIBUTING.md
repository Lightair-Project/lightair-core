# Contributing

Thanks for wanting to help. Before you send a change, please read this: Lightair Core is
dual-licensed (`LICENSING.md`), so contributions work a little differently from a plain GPL
project.

## The contributor licence agreement

A contribution can be merged only after its author has agreed to the Lightair Contributor
Licence Agreement (`docs/CLA.md`). In short, you keep the copyright in your work, and you give
the copyright holder of Lightair Core the right to license your contribution under the GPL and
under other terms, including the commercial licence that Lightair's own apps use.

Without that agreement, a contribution can't be used in all of Lightair's products, so it can't
be merged, however good it is. Until the agreement is published in its final form, outside
contributions aren't being merged; issues, ideas and bug reports are very welcome.

## What a good change looks like

- One idea per change, with a commit message that says what changes and why.
- Tests for what it changes; `cargo xtask check` passes (clippy and every test).
- Only the files the change needs; formatting changes only in files the change touches.
- No code copied from elsewhere. If a file comes from another project, say where it comes from
  and under which licence, and expect to be asked whether it can be used on both sets of terms
  (permissive licences such as MIT and Apache-2.0 can; GPL code by others can't).
- The desktop app (`Lightair-Project/lightair`) keeps building and its tests keep passing, since it
  builds on these crates.

## Reporting a problem

Open an issue with what you did, what you expected and what happened instead. Security problems:
open an issue asking for a private contact, without details.
