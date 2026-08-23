# Bundled fonts

- `Inter.ttf` — Inter, variable font (`opsz`, `wght` axes), mirrored from the
  [Google Fonts repository](https://github.com/google/fonts/tree/main/ofl/inter).
  Registered three times at build time under distinct `wght` coordinates
  (Regular/Medium/SemiBold) via `egui`'s `FontTweak::coords`, so a single file
  covers the UI's proportional text weights.
- `JetBrainsMono.ttf` — JetBrains Mono, variable font (`wght` axis), mirrored
  from the same Google Fonts repository. Used for the monospace family
  (transcript tool output, code).
- `NotoSansSymbols2-Regular.ttf` — Noto Sans Symbols 2, bundled as the
  fallback for UI symbols that Inter does not contain. Its license is included
  in `NotoSansSymbols2-OFL.txt`.

Inter and JetBrains Mono are licensed under the SIL Open Font License 1.1; see
`Inter-OFL.txt` and `JetBrainsMono-OFL.txt`. Noto Sans Symbols 2 is also
licensed under the SIL Open Font License 1.1.
