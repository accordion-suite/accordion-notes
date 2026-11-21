# Accordion Notes

Notes for Accordion: Markdown notes with checklists, labels, colors, folders, reminders, images, and voice memos, kept in Drive, as in Google Keep. Shared notes are edited together, live, and voice memos can be transcribed on the server. Importing from Google Keep is planned.

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

- **The title is the file's name -** A note without a title is named after its first words.
- **Labels are `tags`,** which Obsidian shows as tags. Colors use the names Google Keep's Takeout uses. An archived note has `archived: true`.
- **Folders are Drive folders** in the Notes place, so a note moved in Drive or Obsidian moves in Notes too.
- **Notes changes only the lines of the fields it sets -** Fields it doesn't know, such as Obsidian's own properties, stay as they were.
- **Notes keeps an index** of every note in its own tables, so lists are quick on a Raspberry Pi. It follows each person's changes through core's list of changes, the way a synced device does, so notes edited in Obsidian or over WebDAV show up too.
- **Deleting a note** moves its file to Drive's trash.
- **Sharing a note shares its file,** as in Drive. Everyone who has a shared note open edits it together through core's co-editing service ([decision 30](https://github.com/accordion-suite/accordion/blob/main/docs/decisions/0030-documents-and-co-editing.md)), which writes the file. Its document holds the front matter and the body as two texts, so the front matter stays exactly as it was. A note's color is in its file, but each person's pins, archive, and labels for a note someone else shared are kept in Notes' tables.
- **Images and voice memos are files beside their note,** named after it, and linked from the top of the note by their path from the top of the Notes place, such as `![](Garden/Groceries%201.jpg)`. Obsidian shows them, and Notes also reads Obsidian's own `![[name.jpg]]` and links from the note's folder. When an image moves or is renamed anywhere, Notes rewrites the links to it (`src/links.rs`). Anyone who can open a note sees its images and hears its memos through Notes, so they're never shared themselves. Memos are WebM, MP4, or Ogg sound, as the browser records them.
- **Transcripts** are added to the end of the note, as the person who recorded the memo, when an admin has turned them on and the memo was recorded for one. Core runs whisper.cpp in the sandbox ([decision 33](https://github.com/accordion-suite/accordion/blob/main/docs/decisions/0033-transcribing-voice-memos.md)).
- **Reminders are each person's own,** the owner's too, so they're kept in Notes' tables, never in the file. Each keeps its clock time and time zone, so one that repeats rings at the same time before and after the clocks change. Notes checks for reminders that are due at the start of every minute, and rings them in the person's bell.
- **List settings,** such as whether new items go at the top or bottom, are each person's own, in Settings under **Notes** > **Lists**.

| Path | What it's for | How it signs in |
|---|---|---|
| `/api/notes/` | The JSON API, described in `src/api.rs`, with notes, labels, trash, reminders, settings, and transcripts | Session cookie or bearer token |
| `/api/notes/notes/{id}/image` | A note's image or memo, by its path, with byte ranges for players | Session cookie or bearer token |
| `/api/notes/notes/{id}/images`, `/memos` | Adding an image or a voice memo, up to 50 MB | Session cookie or bearer token |
| `/api/notes/notes/{id}/live` | A WebSocket for editing a shared note together, with Yjs's sync and awareness messages | Session cookie or bearer token, and a matching `Origin` from a browser |

## Building

Install Rust with [rustup](https://rustup.rs), then run these commands from the repo root:

```sh
cargo build --workspace
cargo test --workspace
```

To build against your own checkout of `accordion-core`, clone the workspace with `scripts/clone-workspace.sh` in the `accordion` repo. Before you commit `Cargo.lock`, push `accordion-core` and run `scripts/pin-versions.sh ../accordion-notes` from the `accordion` repo.

## License

[AGPL-3.0 or later](LICENSE)
