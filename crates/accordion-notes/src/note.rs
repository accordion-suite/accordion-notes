//! A note's file: Markdown, with YAML front matter at the top for what
//! Notes keeps about it (decision 31 in the `accordion` repo's
//! `docs/decisions`):
//!
//! ```markdown
//! ---
//! color: yellow
//! pinned: true
//! tags:
//!   - Home
//! ---
//! - [ ] Milk
//! ```
//!
//! Obsidian and other editors write front matter too, so Notes changes
//! only the lines of the fields it sets, and keeps everything else as it
//! was, including fields it doesn't know and the way unchanged fields are
//! written. It reads the small part of YAML that these fields use, rather
//! than all of YAML.

/// The colors a note can have, named as Google Keep's Takeout names them,
/// so imported notes keep theirs. The web app has a light and a dark shade
/// of each.
pub const COLORS: [&str; 11] = [
    "red",
    "orange",
    "yellow",
    "green",
    "teal",
    "blue",
    "dark-blue",
    "purple",
    "pink",
    "brown",
    "gray",
];

/// What Notes keeps about a note in its front matter.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Details {
    /// One of [`COLORS`], or `None` for the plain card.
    pub color: Option<String>,
    pub pinned: bool,
    pub archived: bool,
    /// Labels, which Obsidian shows as tags.
    pub tags: Vec<String>,
}

/// A note's file, read so it can be written back with only the changed
/// parts different.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NoteFile {
    /// The front matter's fields, or `None` if the file has none.
    front: Option<Vec<Entry>>,
    body: String,
    /// The file's line ending, kept for the lines Notes writes.
    newline: &'static str,
}

/// One top-level field of the front matter, with every line that belongs
/// to it, such as a list's items.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Entry {
    /// `None` for lines before the first field, such as comments.
    key: Option<String>,
    lines: Vec<String>,
}

/// A field's value, as far as Notes reads it.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Value {
    Scalar(String),
    List(Vec<String>),
    /// Something else, such as a nested map. Notes leaves it alone.
    Other,
}

impl NoteFile {
    /// A new note with no front matter.
    pub fn new(body: &str) -> Self {
        Self {
            front: None,
            body: body.to_owned(),
            newline: "\n",
        }
    }

    /// Reads a note's file. Every file is a note, so this never fails:
    /// front matter that doesn't close is part of the body.
    pub fn parse(text: &str) -> Self {
        let text = text.strip_prefix('\u{feff}').unwrap_or(text);
        let newline = if text.contains("\r\n") { "\r\n" } else { "\n" };
        let plain = Self {
            front: None,
            body: text.to_owned(),
            newline,
        };
        let mut lines = text.split_inclusive('\n');
        match lines.next() {
            Some(first) if first.trim_end() == "---" => {}
            _ => return plain,
        }
        let mut inside = Vec::new();
        let mut used = text.split_inclusive('\n').next().map_or(0, str::len);
        let mut closed = false;
        for line in lines {
            used += line.len();
            let bare = line.trim_end_matches(['\n', '\r']);
            if bare.trim_end() == "---" || bare.trim_end() == "..." {
                closed = true;
                break;
            }
            inside.push(bare.to_owned());
        }
        if !closed {
            return plain;
        }
        Self {
            front: Some(group(inside)),
            body: text[used..].to_owned(),
            newline,
        }
    }

    pub fn body(&self) -> &str {
        &self.body
    }

    pub fn set_body(&mut self, body: &str) {
        self.body = body.to_owned();
    }

    /// What the front matter says about the note. Fields Notes can't read
    /// count as not set.
    pub fn details(&self) -> Details {
        let color = match self.value("color") {
            Some(Value::Scalar(color)) => {
                let color = color.to_ascii_lowercase();
                COLORS.contains(&color.as_str()).then_some(color)
            }
            _ => None,
        };
        Details {
            color,
            pinned: self.flag("pinned"),
            archived: self.flag("archived"),
            tags: match self.value("tags") {
                Some(Value::List(items)) => clean_tags(items),
                Some(Value::Scalar(text)) => clean_tags(
                    text.split([',', ' '])
                        .map(str::to_owned)
                        .collect::<Vec<_>>(),
                ),
                _ => Vec::new(),
            },
        }
    }

    /// Changes the front matter to say `details`. Fields whose value
    /// doesn't change keep their lines exactly.
    pub fn set_details(&mut self, details: &Details) {
        let now = self.details();
        let tags = clean_tags(details.tags.clone());
        let changes: [(&str, bool, Option<Vec<String>>); 4] = [
            (
                "color",
                now.color != details.color,
                details
                    .color
                    .as_ref()
                    .map(|color| vec![format!("color: {}", scalar(color))]),
            ),
            (
                "pinned",
                now.pinned != details.pinned,
                details.pinned.then(|| vec!["pinned: true".to_owned()]),
            ),
            (
                "archived",
                now.archived != details.archived,
                details.archived.then(|| vec!["archived: true".to_owned()]),
            ),
            (
                "tags",
                now.tags != tags,
                (!tags.is_empty()).then(|| {
                    let mut lines = vec!["tags:".to_owned()];
                    lines.extend(tags.iter().map(|tag| format!("  - {}", scalar(tag))));
                    lines
                }),
            ),
        ];
        for (key, changed, lines) in changes {
            if changed {
                self.set_field(key, lines);
            }
        }
        if self.front.as_ref().is_some_and(Vec::is_empty) {
            self.front = None;
        }
    }

    /// The file's text.
    pub fn text(&self) -> String {
        let mut text = String::new();
        if let Some(entries) = &self.front {
            text.push_str("---");
            text.push_str(self.newline);
            for line in entries.iter().flat_map(|entry| &entry.lines) {
                text.push_str(line);
                text.push_str(self.newline);
            }
            text.push_str("---");
            text.push_str(self.newline);
        }
        text.push_str(&self.body);
        text
    }

    fn entry(&self, key: &str) -> Option<&Entry> {
        self.front
            .as_ref()?
            .iter()
            .find(|entry| entry.key.as_deref() == Some(key))
    }

    fn value(&self, key: &str) -> Option<Value> {
        self.entry(key).map(read_value)
    }

    fn flag(&self, key: &str) -> bool {
        matches!(
            self.value(key),
            Some(Value::Scalar(text))
                if matches!(text.to_ascii_lowercase().as_str(), "true" | "yes" | "on")
        )
    }

    /// Replaces a field's lines, adds the field at the end, or removes it
    /// with `None`.
    fn set_field(&mut self, key: &str, lines: Option<Vec<String>>) {
        let entries = self.front.get_or_insert_with(Vec::new);
        let at = entries
            .iter()
            .position(|entry| entry.key.as_deref() == Some(key));
        match (at, lines) {
            (Some(at), Some(lines)) => entries[at].lines = lines,
            (Some(at), None) => {
                entries.remove(at);
            }
            (None, Some(lines)) => entries.push(Entry {
                key: Some(key.to_owned()),
                lines,
            }),
            (None, None) => {}
        }
    }
}

/// Splits the front matter's lines into top-level fields.
fn group(lines: Vec<String>) -> Vec<Entry> {
    let mut entries: Vec<Entry> = Vec::new();
    for line in lines {
        match field_key(&line) {
            Some(key) => entries.push(Entry {
                key: Some(key),
                lines: vec![line],
            }),
            None => match entries.last_mut() {
                Some(entry) => entry.lines.push(line),
                None => entries.push(Entry {
                    key: None,
                    lines: vec![line],
                }),
            },
        }
    }
    entries
}

/// The key of a line that starts a top-level field, such as `tags` in
/// `tags: [a, b]`.
fn field_key(line: &str) -> Option<String> {
    if line.starts_with([' ', '\t', '-', '#']) || line.is_empty() {
        return None;
    }
    let (key, _) = line.split_once(':')?;
    let key = unquote(key.trim());
    (!key.is_empty()).then_some(key)
}

fn read_value(entry: &Entry) -> Value {
    let first = &entry.lines[0];
    let rest = first.split_once(':').map_or("", |(_, rest)| rest).trim();
    if rest.is_empty() {
        // A block list, with its items on the lines below.
        let mut items = Vec::new();
        for line in &entry.lines[1..] {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            match line.strip_prefix('-') {
                Some(item) if item.is_empty() || item.starts_with(' ') => {
                    items.push(plain_scalar(item.trim()));
                }
                _ => return Value::Other,
            }
        }
        return if items.is_empty() {
            Value::Scalar(String::new())
        } else {
            Value::List(items)
        };
    }
    if entry.lines.len() > 1
        && entry.lines[1..]
            .iter()
            .any(|line| !line.trim().is_empty() && !line.trim().starts_with('#'))
    {
        return Value::Other;
    }
    if let Some(inner) = rest.strip_prefix('[') {
        let Some(inner) = without_comment(inner).trim_end().strip_suffix(']') else {
            return Value::Other;
        };
        return Value::List(
            split_flow(inner)
                .into_iter()
                .map(|item| plain_scalar(item.trim()))
                .filter(|item| !item.is_empty())
                .collect(),
        );
    }
    if rest.starts_with(['{', '|', '>', '&', '*', '!']) {
        return Value::Other;
    }
    Value::Scalar(plain_scalar(rest))
}

/// A scalar as written, without its quotes, or without a trailing comment
/// if it has no quotes.
fn plain_scalar(text: &str) -> String {
    if text.starts_with(['"', '\'']) {
        unquote(text)
    } else {
        without_comment(text).trim().to_owned()
    }
}

fn without_comment(text: &str) -> &str {
    match text.find(" #") {
        Some(at) => &text[..at],
        None if text.starts_with('#') => "",
        None => text,
    }
}

fn unquote(text: &str) -> String {
    if let Some(inner) = text.strip_prefix('"').and_then(|t| t.strip_suffix('"')) {
        let mut out = String::new();
        let mut chars = inner.chars();
        while let Some(c) = chars.next() {
            if c == '\\' {
                match chars.next() {
                    Some('n') => out.push('\n'),
                    Some('t') => out.push('\t'),
                    Some(other) => out.push(other),
                    None => {}
                }
            } else {
                out.push(c);
            }
        }
        return out;
    }
    if let Some(inner) = text.strip_prefix('\'').and_then(|t| t.strip_suffix('\'')) {
        return inner.replace("''", "'");
    }
    text.to_owned()
}

/// Splits a flow list's inside at commas outside quotes.
fn split_flow(inner: &str) -> Vec<&str> {
    let mut parts = Vec::new();
    let mut quote = None;
    let mut start = 0;
    for (at, c) in inner.char_indices() {
        match (quote, c) {
            (None, '"' | '\'') => quote = Some(c),
            (Some(open), _) if c == open => quote = None,
            (None, ',') => {
                parts.push(&inner[start..at]);
                start = at + 1;
            }
            _ => {}
        }
    }
    parts.push(&inner[start..]);
    parts
}

/// Labels without Obsidian's `#`, blanks, or repeats, in their order.
fn clean_tags(items: Vec<String>) -> Vec<String> {
    let mut tags: Vec<String> = Vec::new();
    for item in items {
        let tag = item.trim().trim_start_matches('#').trim();
        if !tag.is_empty() && !tags.iter().any(|seen| seen.eq_ignore_ascii_case(tag)) {
            tags.push(tag.to_owned());
        }
    }
    tags
}

/// A scalar written so YAML reads it back as the same text.
fn scalar(text: &str) -> String {
    let lower = text.to_ascii_lowercase();
    let needs_quotes = text.is_empty()
        || text != text.trim()
        || text.starts_with([
            '-', '?', ':', ',', '[', ']', '{', '}', '#', '&', '*', '!', '|', '>', '\'', '"', '%',
            '@', '`',
        ])
        || text.contains(": ")
        || text.ends_with(':')
        || text.contains(" #")
        || text.contains(['\n', '\t', '\\'])
        || matches!(
            lower.as_str(),
            "true" | "false" | "yes" | "no" | "on" | "off" | "null" | "~"
        )
        || text.parse::<f64>().is_ok();
    if !needs_quotes {
        return text.to_owned();
    }
    let mut quoted = String::from('"');
    for c in text.chars() {
        match c {
            '"' => quoted.push_str("\\\""),
            '\\' => quoted.push_str("\\\\"),
            '\n' => quoted.push_str("\\n"),
            '\t' => quoted.push_str("\\t"),
            c => quoted.push(c),
        }
    }
    quoted.push('"');
    quoted
}

/// Whether a file with this name is a note: a Markdown file that isn't
/// hidden.
pub fn is_note(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    !name.starts_with('.') && (lower.ends_with(".md") || lower.ends_with(".markdown"))
}

/// A note's title: its file's name, without the extension.
pub fn title_of(name: &str) -> &str {
    let lower = name.to_ascii_lowercase();
    for extension in [".md", ".markdown"] {
        if lower.ends_with(extension) {
            return &name[..name.len() - extension.len()];
        }
    }
    name
}

/// The longest title Notes gives a file, in characters, so the name fits
/// with its extension and a number.
const TITLE_CHARS: usize = 100;
/// How much of the first line names a note without a title.
const DERIVED_CHARS: usize = 40;

/// The name a note without a title gets: its first words. The pictures and
/// memos at its top aren't words, so a note of only those is untitled.
pub fn derived_title(body: &str) -> String {
    let line = body
        .lines()
        .map(str::trim)
        .filter(|line| !only_shows_a_file(line))
        .map(strip_markers)
        .find(|line| !line.is_empty())
        .unwrap_or_default();
    let line = safe(&line);
    if line.is_empty() {
        return "Untitled note".to_owned();
    }
    if line.chars().count() <= DERIVED_CHARS {
        return line;
    }
    // Cut at the last space that fits, so a word isn't split.
    let cut: String = line.chars().take(DERIVED_CHARS).collect();
    match cut.rfind(' ') {
        Some(at) if at > 0 => cut[..at].trim_end().to_owned(),
        _ => cut,
    }
}

/// Whether a line does nothing but show a picture or play a memo, such as
/// `![](Garden/Groceries%201.jpg)` or `![[memo.webm]]`.
fn only_shows_a_file(line: &str) -> bool {
    let line = line.trim();
    if !line.starts_with("![") || !(line.ends_with(')') || line.ends_with("]]")) {
        return false;
    }
    matches!(
        crate::links::links(line).as_slice(),
        [link] if link.embed && crate::links::media_extension(&link.target).is_some()
    )
}

/// A line without Markdown's list, checkbox, heading, and quote markers,
/// or the marks around bold, italic, underline, and code.
fn strip_markers(line: &str) -> String {
    let mut line = line;
    loop {
        let before = line;
        for marker in [
            "- [ ] ", "- [x] ", "- [X] ", "* [ ] ", "* [x] ", "- ", "* ", "+ ", "> ",
        ] {
            if let Some(rest) = line.strip_prefix(marker) {
                line = rest.trim_start();
            }
        }
        let hashes = line.len() - line.trim_start_matches('#').len();
        if hashes > 0 && line[hashes..].starts_with(' ') {
            line = line[hashes..].trim_start();
        }
        let digits = line.len() - line.trim_start_matches(|c: char| c.is_ascii_digit()).len();
        if digits > 0 && line[digits..].starts_with(". ") {
            line = line[digits + 2..].trim_start();
        }
        if line == before {
            break;
        }
    }
    line.replace(['*', '_', '`'], "")
        .replace("<u>", "")
        .replace("</u>", "")
        .trim()
        .to_owned()
}

/// Text that can be part of a file's name on every computer the note syncs
/// to: no slashes, no characters Windows refuses, one line.
fn safe(text: &str) -> String {
    let replaced: String = text
        .chars()
        .map(|c| match c {
            '/' | '\\' => '-',
            ':' | '*' | '?' | '"' | '<' | '>' | '|' => ' ',
            c if c.is_control() => ' ',
            c => c,
        })
        .collect();
    let mut words = replaced.split_whitespace().collect::<Vec<_>>().join(" ");
    // Windows doesn't allow a name to end with a dot or a space.
    while words.ends_with(['.', ' ']) {
        words.pop();
    }
    words
}

/// The file name for a note: its title, or its first words without one.
pub fn file_name(title: &str, body: &str) -> String {
    let title: String = safe(title).chars().take(TITLE_CHARS).collect();
    let title = title.trim();
    if title.is_empty() {
        format!("{}.md", derived_title(body))
    } else {
        format!("{title}.md")
    }
}

/// Whether a note's title was given by someone, rather than taken from its
/// first words, which the card then doesn't repeat. A number added to make
/// the name unique, such as " (2)", doesn't count.
pub fn has_title(name: &str, body: &str) -> bool {
    let title = title_of(name);
    let base = match title.rsplit_once(" (") {
        Some((base, number))
            if number
                .strip_suffix(')')
                .is_some_and(|n| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit())) =>
        {
            base
        }
        _ => title,
    };
    base != derived_title(body)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_a_note_after_words_not_its_pictures_or_memos() {
        assert_eq!(
            derived_title("![](Untitled%20note%201.webm)\n"),
            "Untitled note"
        );
        assert_eq!(
            derived_title("![](Trip%201.jpg)\n![[Trip 2.webm]]\n\nBags to pack\n"),
            "Bags to pack"
        );
        // A picture with words beside it, or a link to something else, is
        // words.
        assert_eq!(derived_title("![](a.jpg) Sunset\n"), "![](a.jpg) Sunset");
        assert_eq!(derived_title("![](plan.pdf)\n"), "![](plan.pdf)");
    }

    #[test]
    fn reads_a_note_without_front_matter() {
        let note = NoteFile::parse("- [ ] Milk\n- [x] Bread\n");
        assert_eq!(note.details(), Details::default());
        assert_eq!(note.body(), "- [ ] Milk\n- [x] Bread\n");
        assert_eq!(note.text(), "- [ ] Milk\n- [x] Bread\n");
    }

    #[test]
    fn reads_what_notes_keeps() {
        let note = NoteFile::parse(
            "---\ncolor: Yellow\npinned: true\narchived: no\ntags:\n  - Home\n  - \"#Groceries\"\n---\nMilk\n",
        );
        assert_eq!(
            note.details(),
            Details {
                color: Some("yellow".into()),
                pinned: true,
                archived: false,
                tags: vec!["Home".into(), "Groceries".into()],
            }
        );
        assert_eq!(note.body(), "Milk\n");
    }

    #[test]
    fn reads_tags_however_obsidian_writes_them() {
        for front in [
            "tags: [home, \"big, plans\"]",
            "tags:\n- home\n- big, plans",
            "tags:\n  - home # where\n  - 'big, plans'",
        ] {
            let note = NoteFile::parse(&format!("---\n{front}\n---\n"));
            assert_eq!(note.details().tags, ["home", "big, plans"], "{front}");
        }
        let note = NoteFile::parse("---\ntags: home, work\n---\n");
        assert_eq!(note.details().tags, ["home", "work"]);
    }

    #[test]
    fn keeps_everything_it_doesnt_change() {
        let text = "---\r\ntitle: Trip\r\naliases: [Holiday]\r\ncolor: blue\r\ncssclasses:\r\n  - wide\r\n---\r\nPack the tent.\r\n";
        let mut note = NoteFile::parse(text);
        assert_eq!(note.text(), text);
        note.set_details(&Details {
            color: Some("blue".into()),
            pinned: true,
            ..Details::default()
        });
        assert_eq!(
            note.text(),
            "---\r\ntitle: Trip\r\naliases: [Holiday]\r\ncolor: blue\r\ncssclasses:\r\n  - wide\r\npinned: true\r\n---\r\nPack the tent.\r\n"
        );
    }

    #[test]
    fn writes_front_matter_only_when_something_is_set() {
        let mut note = NoteFile::new("Call the dentist\n");
        note.set_details(&Details {
            color: Some("teal".into()),
            tags: vec!["Health".into(), "To do: soon".into()],
            ..Details::default()
        });
        assert_eq!(
            note.text(),
            "---\ncolor: teal\ntags:\n  - Health\n  - \"To do: soon\"\n---\nCall the dentist\n"
        );
        let reread = NoteFile::parse(&note.text());
        assert_eq!(reread.details().tags, ["Health", "To do: soon"]);

        note.set_details(&Details::default());
        assert_eq!(note.text(), "Call the dentist\n");
    }

    #[test]
    fn leaves_fields_it_cant_read() {
        let text = "---\ncolor:\n  light: red\ntags: {a: b}\n---\nx\n";
        let mut note = NoteFile::parse(text);
        assert_eq!(note.details(), Details::default());
        note.set_details(&Details::default());
        assert_eq!(note.text(), text);
    }

    #[test]
    fn front_matter_that_doesnt_close_is_text() {
        let text = "---\nThis is a divider, not front matter.\n";
        let note = NoteFile::parse(text);
        assert_eq!(note.body(), text);
        assert_eq!(note.details(), Details::default());
    }

    #[test]
    fn quotes_what_yaml_would_read_differently() {
        assert_eq!(scalar("Home"), "Home");
        assert_eq!(scalar("yes"), "\"yes\"");
        assert_eq!(scalar("2026"), "\"2026\"");
        assert_eq!(scalar("#idea"), "\"#idea\"");
        assert_eq!(scalar("say \"hi\""), "say \"hi\"");
        assert_eq!(scalar("- list"), "\"- list\"");
        assert_eq!(unquote(&scalar("a \"b\" \\ c: d")), "a \"b\" \\ c: d");
    }

    #[test]
    fn names_notes_without_a_title_after_their_first_words() {
        assert_eq!(
            derived_title("\n\n- [ ] **Milk** and bread\n"),
            "Milk and bread"
        );
        assert_eq!(
            derived_title("# Ideas: 2027/2028?\nmore"),
            "Ideas 2027-2028"
        );
        assert_eq!(
            derived_title("Remember to water the plants on the balcony every Sunday"),
            "Remember to water the plants on the"
        );
        assert_eq!(derived_title("  \n"), "Untitled note");
        assert_eq!(file_name("", "Milk"), "Milk.md");
        assert_eq!(file_name("Trip / plan.", "x"), "Trip - plan.md");
    }

    #[test]
    fn tells_given_titles_from_first_words() {
        assert!(!has_title("Milk and bread.md", "Milk and bread\nEggs"));
        assert!(!has_title("Milk and bread (2).md", "Milk and bread"));
        assert!(has_title("Shopping.md", "Milk and bread"));
        assert!(!has_title("Untitled note.md", ""));
        assert!(is_note("Trip.md") && is_note("Old.MARKDOWN"));
        assert!(!is_note(".hidden.md") && !is_note("photo.jpg"));
        assert_eq!(title_of("Trip.MD"), "Trip");
    }
}
