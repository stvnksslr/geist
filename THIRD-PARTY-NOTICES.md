# Third-party notices

giest is distributed under the MIT License (see [`LICENSE`](LICENSE)). The
distributed `giest.exe` also embeds or links the following third-party
components, included here under their respective licenses.

## JetBrains Mono Nerd Font

The primary terminal font is embedded directly into the binary (via
`include_bytes!`). JetBrains Mono is licensed under the **SIL Open Font License,
Version 1.1**. The full license text ships in the repository at
[`assets/fonts/OFL.txt`](assets/fonts/OFL.txt).

- JetBrains Mono — Copyright 2020 The JetBrains Mono Project Authors
- Nerd Fonts patching — https://github.com/ryanoasis/nerd-fonts

## libghostty-rs

The safe Rust bindings to Ghostty's VT engine are vendored under
`vendor/libghostty-rs/` and linked into the binary. Licensed under the **MIT
License** (see [`vendor/libghostty-rs/LICENSE`](vendor/libghostty-rs/LICENSE)).

- Copyright (c) 2026 Uzair Aftab, Leah Amelia Chen
- Upstream: https://github.com/Uzaaft/libghostty-rs

## Ghostty (libghostty-vt)

Ghostty's terminal VT library is compiled from source (at a pinned commit) by
the `libghostty-vt-sys` build script and statically linked into the binary.
Ghostty is licensed under the **MIT License**.

- Upstream: https://github.com/ghostty-org/ghostty

## Other dependencies

Remaining Rust crate dependencies (eframe/egui, wgpu, portable-pty, rustybuzz,
ab_glyph, arboard, and others) are licensed under permissive MIT/Apache-2.0
terms. See each crate's repository for details.
