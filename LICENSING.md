# Licensing

Copyright © 2026 Bregas S. Wicaksono (MoonGoblin).

Lightair Core is **dual-licensed**. You may use it under either:

1. **The GNU General Public License, version 3 or (at your option) any later version**
   (`GPL-3.0-or-later`, the text is in `LICENSE`). This is the licence everyone gets: use it,
   study it, change it and share it, on the GPL's terms, which include publishing the source of
   anything you distribute that's built on it.
2. **A commercial licence from the copyright holder**, for use the GPL doesn't allow, such as in
   an application whose own source isn't published. Ask the copyright holder through GitHub
   (https://github.com/MoonGoblinDev).

## Who holds the copyright

Bregas S. Wicaksono, who publishes as MoonGoblin, wrote all of Lightair Core and holds its
copyright. The licence audit (`docs/licence-audit.md`) records that the repository contains no code by anyone else, apart
from a few files under permissive licences that keep their own notices (jQuery, MIT; the Inter
font used by tests, SIL OFL 1.1).

## How Lightair uses it

| Product | Licence it uses Lightair Core under |
|---|---|
| Lightair, the desktop app ([Lightair-Project/lightair](https://github.com/Lightair-Project/lightair)) | GPL-3.0-or-later (the desktop app is GPL itself, because it's built on libobs) |
| Lightair Live and Lightair Editor, the mobile apps | The copyright holder's commercial terms |

This is the same model as Qt and other dual-licensed projects: the code is open under the GPL,
and its owner also uses and licenses it on other terms. The GPL applies to people who receive
the code under it; it doesn't restrict the copyright holder.

## Contributions

Because the copyright holder licenses Lightair Core on two sets of terms, contributions can only
be accepted from people who agree that their work may be licensed both ways. See
`CONTRIBUTING.md`.

## Third-party parts

Files in this repository written by others keep their own licences and notices; each is listed in
`docs/licence-audit.md` §3. Dependencies are listed by `cargo metadata` and keep their own
licences; none is GPL-, LGPL- or AGPL-only.
