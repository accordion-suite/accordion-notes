//! A note as a co-edited document (decision 30): two texts, `front` for the
//! file's front matter exactly as it is, and `body` for what people type.
//!
//! The front matter stays text rather than fields, so properties from
//! Obsidian keep their order and spelling. The web app never edits it: a
//! color or a label changes through Notes' API, which writes the file, and
//! the change reaches the document like any change from outside.

use accordion_core::coedit::{Format, FormatError, set_text};
use yrs::{Doc, GetString, Transact};

use crate::note::NoteFile;

pub(crate) struct NoteFormat;

impl Format for NoteFormat {
    fn name(&self) -> &'static str {
        "notes"
    }

    fn absorb(&self, doc: &Doc, contents: &[u8]) -> Result<(), FormatError> {
        let text = std::str::from_utf8(contents)
            .map_err(|_| FormatError("the note isn't UTF-8 text".into()))?;
        let (front, body) = split(text);
        let front_text = doc.get_or_insert_text("front");
        let body_text = doc.get_or_insert_text("body");
        let mut txn = doc.transact_mut();
        set_text(&mut txn, &front_text, front);
        set_text(&mut txn, &body_text, body);
        Ok(())
    }

    fn write(&self, doc: &Doc) -> Result<Vec<u8>, FormatError> {
        let front = doc.get_or_insert_text("front");
        let body = doc.get_or_insert_text("body");
        let txn = doc.transact();
        let mut text = front.get_string(&txn);
        text.push_str(&body.get_string(&txn));
        Ok(text.into_bytes())
    }
}

/// The file's front matter, with its `---` lines and anything before it,
/// and its body. Together they're the whole file.
pub(crate) fn split(text: &str) -> (&str, &str) {
    let file = NoteFile::parse(text);
    let body_len = file.body().len();
    text.split_at(text.len() - body_len)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits_the_front_matter_from_the_body() {
        let text = "---\ncolor: yellow\naliases: [x]\n---\n- [ ] Milk\n";
        assert_eq!(
            split(text),
            ("---\ncolor: yellow\naliases: [x]\n---\n", "- [ ] Milk\n")
        );
        assert_eq!(split("Just text"), ("", "Just text"));
        assert_eq!(split("\u{feff}Text"), ("\u{feff}", "Text"));
        // Front matter that doesn't close is part of the body.
        assert_eq!(split("---\nnot closed\n"), ("", "---\nnot closed\n"));
    }

    #[test]
    fn writes_back_what_it_read() {
        let doc = Doc::with_options(yrs::Options {
            offset_kind: yrs::OffsetKind::Bytes,
            ..yrs::Options::default()
        });
        let text = "---\r\ntags: [Home]\r\n---\r\nCafé ☕ list\r\n";
        NoteFormat.absorb(&doc, text.as_bytes()).unwrap();
        assert_eq!(NoteFormat.write(&doc).unwrap(), text.as_bytes());
        let body = doc.get_or_insert_text("body");
        assert_eq!(body.get_string(&doc.transact()), "Café ☕ list\r\n");
        let changed = "---\r\ntags: [Home, Shop]\r\n---\r\nCafé ☕ list, tea\r\n";
        NoteFormat.absorb(&doc, changed.as_bytes()).unwrap();
        assert_eq!(NoteFormat.write(&doc).unwrap(), changed.as_bytes());
    }
}
