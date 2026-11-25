# lightair-engine: notices

## Google MediaPipe's canonical face model (Apache-2.0)

The face mesh tables in `src/looks/mesh.rs`, the face regions in `src/looks/face.rs` and the three
makeup maps `assets/effect-stack/makeup/face-paint.png`, `face-liner.png` and `face-normal.png`
are generated from Google MediaPipe's canonical face model (468 vertices, their UVs and
triangles), `mediapipe/modules/face_geometry/data/canonical_face_model.obj` in
https://github.com/google-ai-edge/mediapipe.

Copyright Google LLC. Licensed under the Apache License, Version 2.0; the licence's text is in
`assets/effect-stack/LICENSE-APACHE-2.0`. Changed: the model's geometry turned into Rust tables
and drawn into texture maps for the looks.

The rest of this crate is Lightair's own code and data, under the repository's licence.
