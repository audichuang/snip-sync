# crates/desktop

- HeroUI v3 is not what you remember. Write components from the live v3 docs (`https://v3.heroui.com/docs/react/…`), never from memory. The same rule applies in aghub.
- Tauri commands in `src-tauri/src/commands/` are thin: call `snip-core` and put no logic there. Logic the CLI also needs belongs in `crates/core`.
