# Accordion Notes

Notes for Accordion: Markdown notes with checklists, labels, and colors, kept in Drive, as in Google Keep. Sharing, reminders, and images are planned.

This repo is part of [Accordion](https://github.com/accordion-suite/accordion), a self-hosted, zero-telemetry replacement for Google Workspace. Please open issues in the [main repo](https://github.com/accordion-suite/accordion/issues).

## How it fits together

Notes is a library crate, `accordion-notes`, that the server in the `accordion` repo builds in. It implements the `Module` trait from `accordion-module` ([decision 7](https://github.com/accordion-suite/accordion/blob/main/docs/decisions/0007-module-interface.md)), and requires Drive. [Decision 31](https://github.com/accordion-suite/accordion/blob/main/docs/decisions/0031-notes.md) explains the design.

Each note is a Markdown file in its owner's Notes place, which Drive shows as a tab beside My Drive ([decision 32](https://github.com/accordion-suite/accordion/blob/main/docs/decisions/0032-a-place-for-each-apps-files.md)), so notes sync to computers and open in Obsidian or any editor. What the cards show is in each file's YAML front matter:

```markdown
---
color: yellow
pinned: true
tags:
  - Home
---
- [ ] Milk
- [x] Bread
```

- **The title is the file's name.** A note without a title is named after its first words.
- **Labels are `tags`,** which Obsidian shows as tags. Colors use the names Google Keep's Takeout uses.
- **Notes changes only the lines of the fields it sets.** Fields it doesn't know, such as Obsidian's own properties, stay as they were.
- **Notes keeps an index** of every note in its own tables, so lists are quick on a Raspberry Pi. It follows each person's changes through core's list of changes, the way a synced device does, so notes edited in Obsidian or over WebDAV show up too.
- **Deleting a note** moves its file to Drive's trash.

| Path | What it's for | How it signs in |
|---|---|---|
| `/api/notes/` | The JSON API, described in `src/api.rs` | Session cookie or bearer token |

## Building

Install Rust with [rustup](https://rustup.rs), then run these commands from the repo root:

```sh
cargo build --workspace
cargo test --workspace
```

To build against your own checkout of `accordion-core`, clone the workspace with `scripts/clone-workspace.sh` in the `accordion` repo. Before you commit `Cargo.lock`, push `accordion-core` and run `scripts/pin-versions.sh ../accordion-notes` from the `accordion` repo.

## License

[AGPL-3.0 or later](LICENSE)
