# IntelliJ New UI icons

SVG icons copied unmodified from [JetBrains/intellij-community](https://github.com/JetBrains/intellij-community)
at commit `3da3d944f6aeb98e031194ddc8343b0b0db030e7` (2026-09-27), licensed under
Apache-2.0 (`LICENSE.txt`, `NOTICE.txt` next to this file).

Each icon has a light `name.svg` and a dark `name_dark.svg`. They are embedded by
`src/icons.rs`, which maps `Icon` variants to these files.

| Directory here | Source directory |
| --- | --- |
| `expui/{actions,diff,fileTypes,general,inline,nodes,status,toolwindows,vcs}/` | `platform/icons/src/expui/<same>/` |
| `expui/dvcs/` | `platform/dvcs-impl/shared/resources/icons/new/` |
| `expui/language/rust*.svg` | `platform/icons/src/language/` |

To add an icon, copy both variants from the same commit (or bump the commit
above for all of them) and add a line to the `icons!` table in `src/icons.rs`.
