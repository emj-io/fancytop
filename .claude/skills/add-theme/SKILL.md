---
name: add-theme
description: Add a colour theme to fancytop. Use when asked for a new theme or palette.
---

1. Read `src/theme.rs`: each `Theme` sets background, text, border and accent colours, five memory colours and the `heat`, `gpu`, `mem`, `disk`, `net` and `title` gradients. Copy the closest existing theme and edit it; register it in the `THEMES` list.
2. Theme names must stay unique after lowercasing and dropping non-alphanumerics (`Theme::find` matches that way for `-t`).
3. Keep text readable: `dim` and `faint` must stay visible on `bg`, and `on_accent` readable on `accent`. Check light backgrounds especially.
4. Look at it: `cargo run --release -- -t "<name>" --snapshot 160x50`, and in a real terminal with `c` / `C`.
5. Update the theme count and names in `README.md` ("Colour themes") and add a `CHANGELOG.md` line.
6. Run the `check` skill.
