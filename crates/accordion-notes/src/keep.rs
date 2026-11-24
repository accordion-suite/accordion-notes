//! Google Keep's notes from Google Takeout (decision 34 in the `accordion`
//! repo's `docs/decisions`), as the sandbox reads them.
//!
//! Keep's folder holds a `.json` file per note, an `.html` copy of each,
//! the note's attachments named by number, and `Labels.txt`. Only the
//! `.json` files are read: they have the full title, the dates, and each
//! line's formatting. The folder's name follows the account's language, so
//! the folder is Keep's when most of its `.json` files are notes.
//!
//! Everything here runs in the sandbox on files from the archive, so it
//! reads them as untrusted: sizes are capped, and the server checks the
//! result again before storing it.

use accordion_core::takeout::{FolderEntry, Planned, PlannedFile, Product, ProductFolder};
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Keep, as plans and the report name it.
pub const PRODUCT: &str = "keep";

/// The longest note text Notes takes from Keep, in bytes of Markdown.
/// Keep's own limit is about 20,000 characters.
pub const MAX_BODY: usize = 1024 * 1024;
/// The longest title kept, in characters.
pub const MAX_TITLE: usize = 1000;
/// How many labels, collaborators, links, or attachments a note may have.
pub const MAX_LIST: usize = 100;
/// How many items a checklist may have.
const MAX_ITEMS: usize = 10_000;
/// How much of the title goes into an item's key.
const KEY_TITLE: usize = 500;
/// Keep's list of the person's labels, one a line, including labels no
/// note has.
const LABELS: &str = "Labels.txt";
/// How many labels the list may have.
pub const MAX_LABELS: usize = 1000;

/// Keep, for the reader's list of products.
pub struct Keep;

/// A note, converted, as the server's side stores it.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct KeepNote {
    pub title: String,
    /// The note's text as Markdown, without its attachments.
    pub body: String,
    /// One of Notes' colors, or `None` for Keep's default.
    pub color: Option<String>,
    pub pinned: bool,
    pub archived: bool,
    pub trashed: bool,
    pub labels: Vec<String>,
    /// When it was made and last edited, in microseconds since 1970.
    pub created: i64,
    pub edited: i64,
    /// Whether it had a reminder, which Takeout leaves out.
    pub reminder: bool,
    /// Who it was shared with, by email address.
    pub sharees: Vec<Sharee>,
    /// Attachments the note names that aren't in the archives.
    pub missing: Vec<String>,
    /// Keep's whole list of labels, on the export's first note only, so
    /// labels no note has come over too.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub all_labels: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Sharee {
    pub email: String,
    pub owner: bool,
}

/// What an item is that isn't a note, so the server can say why it didn't
/// come over.
#[derive(Debug, Serialize, Deserialize)]
pub struct NotANote {
    /// Its name in Keep's folder.
    pub name: String,
    pub not_a_note: String,
}

impl Product for Keep {
    fn id(&self) -> &'static str {
        PRODUCT
    }

    fn wants(&self, path: &str) -> bool {
        !path.contains('/') && (path.ends_with(".json") || path == LABELS)
    }

    fn plan(&self, folder: &ProductFolder) -> Option<Vec<Planned>> {
        let by_name: std::collections::HashMap<&str, &FolderEntry> = folder
            .entries
            .iter()
            .filter(|entry| !entry.folder && !entry.path.contains('/'))
            .map(|entry| (entry.path.as_str(), entry))
            .collect();
        let mut planned = Vec::new();
        let mut notes = 0;
        let mut others = 0;
        let mut all_labels = Vec::new();
        let mut first_note = None;
        for entry in folder.entries {
            if entry.folder || !self.wants(&entry.path) {
                continue;
            }
            if entry.path == LABELS {
                all_labels = folder.read(entry).map(labels_list).unwrap_or_default();
                continue;
            }
            let bytes = folder.read(entry);
            let read = bytes.and_then(|bytes| serde_json::from_slice::<Value>(bytes).ok());
            let Some(json) = read.filter(is_note) else {
                others += 1;
                let why = match bytes {
                    None => "it's too large to read as a note",
                    Some(_) => "it isn't a note Accordion can read",
                };
                planned.push(not_a_note(entry, why));
                continue;
            };
            notes += 1;
            first_note.get_or_insert(planned.len());
            planned.push(plan_note(entry, &json, &by_name));
        }
        if let Some(first) = first_note
            && !all_labels.is_empty()
        {
            let fields = &mut planned[first].fields;
            fields["all_labels"] = Value::from(all_labels);
        }
        // Drive's folder has an `-info.json` for nearly every file, so it
        // never looks like Keep's, even with notes kept in it.
        (notes > 0 && notes >= others).then_some(planned)
    }
}

/// Whether `json` is a Keep note: what every note has, its creation time,
/// and text or a checklist.
fn is_note(json: &Value) -> bool {
    json.get("createdTimestampUsec")
        .is_some_and(Value::is_number)
        && (json.get("textContent").is_some_and(Value::is_string)
            || json.get("listContent").is_some_and(Value::is_array))
}

fn not_a_note(entry: &FolderEntry, why: &str) -> Planned {
    Planned {
        key: format!("{} {}", entry.path, entry.size),
        fields: serde_json::to_value(NotANote {
            name: cut(&entry.path, 255),
            not_a_note: why.to_owned(),
        })
        .expect("a string"),
        files: vec![PlannedFile {
            role: "note".to_owned(),
            entry: entry.id,
        }],
    }
}

fn plan_note(
    entry: &FolderEntry,
    json: &Value,
    by_name: &std::collections::HashMap<&str, &FolderEntry>,
) -> Planned {
    let note = convert(json);
    let mut files = vec![PlannedFile {
        role: "note".to_owned(),
        entry: entry.id,
    }];
    let mut missing = Vec::new();
    for attachment in list(json, "attachments") {
        let Some(path) = attachment.get("filePath").and_then(Value::as_str) else {
            continue;
        };
        match by_name.get(path) {
            Some(found) if !files.iter().any(|file| file.entry == found.id) => {
                files.push(PlannedFile {
                    role: "attachment".to_owned(),
                    entry: found.id,
                });
            }
            Some(_) => {}
            None => missing.push(cut(path, 255)),
        }
    }
    // The title and when the note was made tell it apart in later exports,
    // which name its file differently.
    let key = format!("{} {}", note.created, cut(&note.title, KEY_TITLE));
    Planned {
        key,
        fields: serde_json::to_value(KeepNote { missing, ..note }).expect("plain fields"),
        files,
    }
}

/// The note's fields from Keep's JSON.
fn convert(json: &Value) -> KeepNote {
    let text = |key: &str| json.get(key).and_then(Value::as_str).unwrap_or_default();
    let flag = |key: &str| json.get(key).and_then(Value::as_bool).unwrap_or(false);
    let time = |key: &str| json.get(key).and_then(Value::as_i64).unwrap_or(0).max(0);
    let created = time("createdTimestampUsec");
    let body = match json.get("listContent").and_then(Value::as_array) {
        Some(items) => checklist(items),
        None => text_body(
            text("textContent"),
            json.get("textContentHtml").and_then(Value::as_str),
        ),
    };
    let body = with_links(body, json);
    KeepNote {
        title: cut(&one_line(text("title")), MAX_TITLE),
        body: cut(&body, MAX_BODY),
        color: color(text("color")),
        pinned: flag("isPinned"),
        archived: flag("isArchived"),
        trashed: flag("isTrashed"),
        labels: list(json, "labels")
            .filter_map(|label| label.get("name").and_then(Value::as_str))
            .map(|name| cut(&one_line(name), 200))
            .take(MAX_LIST)
            .collect(),
        created,
        edited: match time("userEditedTimestampUsec") {
            0 => created,
            edited => edited,
        },
        reminder: list(json, "tasks").next().is_some(),
        sharees: list(json, "sharees")
            .filter_map(|sharee| {
                Some(Sharee {
                    email: cut(&one_line(sharee.get("email")?.as_str()?), 320),
                    owner: sharee
                        .get("isOwner")
                        .and_then(Value::as_bool)
                        .unwrap_or(false),
                })
            })
            .take(MAX_LIST)
            .collect(),
        missing: Vec::new(),
        all_labels: Vec::new(),
    }
}

/// The labels in Keep's `Labels.txt`, one a line, in its order.
fn labels_list(bytes: &[u8]) -> Vec<String> {
    let mut labels: Vec<String> = Vec::new();
    for line in String::from_utf8_lossy(bytes).lines() {
        let label = cut(line.trim().trim_start_matches('\u{feff}'), 200);
        if !label.is_empty() && !labels.contains(&label) {
            labels.push(label);
        }
        if labels.len() == MAX_LABELS {
            break;
        }
    }
    labels
}

/// The objects in the list `key` of `json`, none if it isn't one.
fn list<'a>(json: &'a Value, key: &str) -> impl Iterator<Item = &'a Value> {
    json.get(key)
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .take(MAX_LIST * 10)
}

/// Keep's color as Notes names it. Keep calls Storm `CERULEAN`, which is
/// Notes' dark blue.
pub fn color(keep: &str) -> Option<String> {
    let color = match keep.to_ascii_uppercase().as_str() {
        "CERULEAN" => "dark-blue".to_owned(),
        other => other.to_ascii_lowercase(),
    };
    crate::note::COLORS
        .contains(&color.as_str())
        .then_some(color)
}

/// A checklist as Markdown task items, in Keep's order. Keep exports no
/// indents, so every item is at the top level.
fn checklist(items: &[Value]) -> String {
    let mut body = String::new();
    for item in items.iter().take(MAX_ITEMS) {
        let text = item.get("text").and_then(Value::as_str).unwrap_or_default();
        let html = item.get("textHtml").and_then(Value::as_str);
        let checked = item
            .get("isChecked")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        // An item is one line, so its own line breaks become spaces.
        let line = text_body(text, html).replace('\n', " ");
        let line = line.trim();
        body.push_str(if checked { "- [x]" } else { "- [ ]" });
        if !line.is_empty() {
            body.push(' ');
            body.push_str(line);
        }
        body.push('\n');
    }
    body
}

/// The note's text as Markdown: from its HTML, for headings, bold, and
/// italics, when the HTML says the same as the plain text, or from the
/// plain text.
fn text_body(text: &str, html: Option<&str>) -> String {
    if let Some(lines) = html.map(read_html)
        && same_text(&plain(&lines), text)
    {
        return markdown(&lines);
    }
    text.split('\n')
        .map(|line| escape_line(line.trim_end_matches('\r')))
        .collect::<Vec<_>>()
        .join("\n")
}

/// Adds Keep's link previews at the end, as links titled with the page's
/// title, unless the text already has the address.
fn with_links(mut body: String, json: &Value) -> String {
    let mut added = false;
    for annotation in list(json, "annotations").take(MAX_LIST) {
        if annotation.get("source").and_then(Value::as_str) != Some("WEBLINK") {
            continue;
        }
        let Some(url) = annotation.get("url").and_then(Value::as_str) else {
            continue;
        };
        let usable = (url.starts_with("https://") || url.starts_with("http://"))
            && url.len() <= 2048
            && !url
                .chars()
                .any(|c| c.is_whitespace() || c.is_control() || matches!(c, '<' | '>'));
        if !usable || body.contains(url) {
            continue;
        }
        let title = annotation
            .get("title")
            .and_then(Value::as_str)
            .map(|title| cut(&one_line(title), 300))
            .filter(|title| !title.trim().is_empty())
            .unwrap_or_else(|| url.to_owned());
        if !added {
            let trimmed = body.trim_end_matches('\n').len();
            body.truncate(trimmed);
            if !body.is_empty() {
                body.push_str("\n\n");
            }
            added = true;
        } else {
            body.push('\n');
        }
        body.push_str(&format!("[{}](<{url}>)", escape_line(title.trim())));
    }
    if added {
        body.push('\n');
    }
    body
}

/// A run of text with the same style.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Run {
    text: String,
    bold: bool,
    italic: bool,
    underline: bool,
}

/// A line of the note: a heading's level, or 0, and its runs.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct Line {
    heading: u8,
    runs: Vec<Run>,
}

/// The styles an element gives what's inside it.
#[derive(Clone, Copy)]
struct Style {
    bold: bool,
    italic: bool,
    underline: bool,
}

/// Reads Keep's HTML into lines of styled text. Keep writes each line as
/// `<p>` or `<h1>`/`<h2>` with `<span style>` runs, and its formatting is
/// in the style: `font-weight:700`, `font-style:italic`, and
/// `text-decoration:underline`. This reads only those few shapes, and
/// anything else as plain text.
fn read_html(html: &str) -> Vec<Line> {
    let mut lines = Vec::new();
    let mut line = Line::default();
    let mut styles: Vec<(String, Style)> = Vec::new();
    let mut skipping: Option<String> = None;
    let mut rest = html;
    let mut text = String::new();
    let style = |styles: &[(String, Style)]| {
        styles.last().map_or(
            Style {
                bold: false,
                italic: false,
                underline: false,
            },
            |(_, style)| *style,
        )
    };
    loop {
        let (before, tag) = match rest.find('<') {
            Some(at) => (&rest[..at], Some(at)),
            None => (rest, None),
        };
        if skipping.is_none() {
            text.push_str(before);
        }
        let Some(at) = tag else {
            break;
        };
        let after = &rest[at + 1..];
        let end = tag_end(after).unwrap_or(after.len());
        let inside = &after[..end];
        rest = after.get(end + 1..).unwrap_or("");
        let (closing, name, attributes) = parse_tag(inside);
        if let Some(skipped) = &skipping {
            if closing && name == *skipped {
                skipping = None;
            }
            continue;
        }
        let current = style(&styles);
        add_text(&mut line, &mut lines, &decode(&text), current);
        text.clear();
        match name.as_str() {
            "script" | "style" | "title" | "head" if !closing => skipping = Some(name),
            "br" => lines.push(std::mem::take(&mut line)),
            "p" | "div" | "li" | "h1" | "h2" | "h3" | "h4" | "h5" | "h6" | "blockquote" | "pre" => {
                if closing {
                    // An empty `<p></p>` is an empty line.
                    if !line.runs.is_empty() || block_open(&styles, &name) {
                        lines.push(std::mem::take(&mut line));
                    }
                    close(&mut styles, &name);
                } else {
                    if !line.runs.is_empty() {
                        lines.push(std::mem::take(&mut line));
                    }
                    line.heading = name
                        .strip_prefix('h')
                        .and_then(|level| level.parse().ok())
                        .unwrap_or(0);
                    styles.push((name, current));
                }
            }
            _ if closing => close(&mut styles, &name),
            "b" | "strong" => styles.push((
                name,
                Style {
                    bold: true,
                    ..current
                },
            )),
            "i" | "em" => styles.push((
                name,
                Style {
                    italic: true,
                    ..current
                },
            )),
            "u" => styles.push((
                name,
                Style {
                    underline: true,
                    ..current
                },
            )),
            "span" | "font" | "a" | "s" | "sup" | "sub" | "code" => {
                let mut style = current;
                if let Some(css) = attribute(attributes, "style") {
                    if let Some(weight) = css_value(&css, "font-weight") {
                        style.bold = match weight.as_str() {
                            "bold" | "bolder" => true,
                            weight => weight.parse::<u32>().is_ok_and(|weight| weight >= 600),
                        };
                    }
                    if let Some(font_style) = css_value(&css, "font-style") {
                        style.italic = font_style == "italic" || font_style == "oblique";
                    }
                    let decoration = css_value(&css, "text-decoration-line")
                        .or_else(|| css_value(&css, "text-decoration"));
                    if let Some(decoration) = decoration {
                        style.underline = decoration.contains("underline");
                    }
                }
                styles.push((name, style));
            }
            _ => {}
        }
        if styles.len() > 1000 {
            styles.remove(0);
        }
    }
    add_text(&mut line, &mut lines, &decode(&text), style(&styles));
    if !line.runs.is_empty() {
        lines.push(line);
    }
    lines
}

/// Whether the block `name` is open, so closing it ends a line even if
/// the line is empty, as `<p></p>` is.
fn block_open(styles: &[(String, Style)], name: &str) -> bool {
    styles.iter().any(|(open, _)| open == name)
}

/// Closes the innermost `name` and what's inside it.
fn close(styles: &mut Vec<(String, Style)>, name: &str) {
    if let Some(at) = styles.iter().rposition(|(open, _)| open == name) {
        styles.truncate(at);
    }
}

/// Adds `text` to the line, starting a new line at each line break, as
/// Keep's `white-space: pre-wrap` shows it.
fn add_text(line: &mut Line, lines: &mut Vec<Line>, text: &str, style: Style) {
    for (index, part) in text.split('\n').enumerate() {
        if index > 0 {
            lines.push(std::mem::take(line));
        }
        if part.is_empty() {
            continue;
        }
        match line.runs.last_mut() {
            Some(run)
                if run.bold == style.bold
                    && run.italic == style.italic
                    && run.underline == style.underline =>
            {
                run.text.push_str(part);
            }
            _ => line.runs.push(Run {
                text: part.to_owned(),
                bold: style.bold,
                italic: style.italic,
                underline: style.underline,
            }),
        }
    }
}

/// Where a tag that starts after `<` ends, at its `>`, skipping quoted
/// attribute values.
fn tag_end(after: &str) -> Option<usize> {
    let mut quote = None;
    for (at, c) in after.char_indices() {
        match (quote, c) {
            (None, '"' | '\'') => quote = Some(c),
            (Some(open), _) if c == open => quote = None,
            (None, '>') => return Some(at),
            _ => {}
        }
    }
    None
}

/// A tag's inside as whether it closes, its name in lowercase, and its
/// attributes.
fn parse_tag(inside: &str) -> (bool, String, &str) {
    let inside = inside.trim_end_matches('/');
    let (closing, inside) = match inside.strip_prefix('/') {
        Some(rest) => (true, rest),
        None => (false, inside),
    };
    let end = inside
        .find(|c: char| c.is_whitespace())
        .unwrap_or(inside.len());
    (closing, inside[..end].to_ascii_lowercase(), &inside[end..])
}

/// The value of the attribute `name`, decoded.
fn attribute(attributes: &str, name: &str) -> Option<String> {
    let mut rest = attributes;
    loop {
        rest = rest.trim_start();
        let key_end = rest.find(|c: char| c == '=' || c.is_whitespace())?;
        let key = &rest[..key_end];
        rest = rest[key_end..].trim_start();
        let Some(after) = rest.strip_prefix('=') else {
            continue;
        };
        let after = after.trim_start();
        let (value, next) = match after.chars().next()? {
            quote @ ('"' | '\'') => {
                let close = after[1..].find(quote)? + 1;
                (&after[1..close], &after[close + 1..])
            }
            _ => {
                let end = after.find(char::is_whitespace).unwrap_or(after.len());
                (&after[..end], &after[end..])
            }
        };
        if key.eq_ignore_ascii_case(name) {
            return Some(decode(value));
        }
        rest = next;
    }
}

/// A property's value in a `style` attribute, in lowercase, the last one
/// winning as in CSS.
fn css_value(css: &str, property: &str) -> Option<String> {
    css.split(';')
        .filter_map(|declaration| declaration.split_once(':'))
        .filter(|(name, _)| name.trim().eq_ignore_ascii_case(property))
        .map(|(_, value)| {
            value
                .trim()
                .trim_end_matches("!important")
                .trim()
                .to_ascii_lowercase()
        })
        .next_back()
}

/// Text with HTML's character references decoded.
fn decode(text: &str) -> String {
    if !text.contains('&') {
        return text.to_owned();
    }
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(at) = rest.find('&') {
        out.push_str(&rest[..at]);
        rest = &rest[at..];
        let end = rest[1..]
            .find(|c: char| !(c.is_ascii_alphanumeric() || c == '#'))
            .map(|end| end + 1)
            .unwrap_or(rest.len());
        let name = &rest[1..end];
        let decoded = match name {
            "amp" => Some('&'),
            "lt" => Some('<'),
            "gt" => Some('>'),
            "quot" => Some('"'),
            "apos" => Some('\''),
            "nbsp" => Some('\u{a0}'),
            _ => name.strip_prefix('#').and_then(|number| {
                let code = match number.strip_prefix(['x', 'X']) {
                    Some(hex) => u32::from_str_radix(hex, 16).ok(),
                    None => number.parse().ok(),
                };
                code.and_then(char::from_u32)
            }),
        };
        match decoded {
            Some(c) => {
                out.push(c);
                rest = &rest[end..];
                rest = rest.strip_prefix(';').unwrap_or(rest);
            }
            None => {
                out.push('&');
                rest = &rest[1..];
            }
        }
    }
    out.push_str(rest);
    out
}

/// The lines' text without their styles.
fn plain(lines: &[Line]) -> String {
    lines
        .iter()
        .map(|line| {
            line.runs
                .iter()
                .map(|run| run.text.as_str())
                .collect::<String>()
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Whether two texts say the same, apart from no-break spaces and spaces
/// or blank lines at the ends.
fn same_text(a: &str, b: &str) -> bool {
    let normal = |text: &str| {
        text.replace('\u{a0}', " ")
            .replace("\r\n", "\n")
            .lines()
            .map(str::trim_end)
            .collect::<Vec<_>>()
            .join("\n")
            .trim_end()
            .to_owned()
    };
    normal(a) == normal(b)
}

/// The lines as Markdown: headings, and bold, italic, and underlined runs.
fn markdown(lines: &[Line]) -> String {
    let mut out = Vec::new();
    for line in lines {
        let mut text = String::new();
        for (index, run) in line.runs.iter().enumerate() {
            let at_start = index == 0;
            let escaped = escape_text(
                &run.text.replace('\u{a0}', " "),
                at_start && line.heading == 0,
            );
            let marks = match (run.bold, run.italic) {
                (true, true) => "***",
                (true, false) => "**",
                (false, true) => "*",
                (false, false) => "",
            };
            let core = escaped.trim();
            if (marks.is_empty() && !run.underline) || core.is_empty() {
                text.push_str(&escaped);
                continue;
            }
            // Markdown's marks can't have spaces just inside them.
            // Underline has no Markdown of its own, so it's HTML's `<u>`,
            // as Notes and Obsidian show it.
            let leading = &escaped[..escaped.len() - escaped.trim_start().len()];
            let trailing = &escaped[escaped.trim_end().len()..];
            let (open, close) = if run.underline {
                ("<u>", "</u>")
            } else {
                ("", "")
            };
            text.push_str(leading);
            text.push_str(open);
            text.push_str(marks);
            text.push_str(core);
            text.push_str(marks);
            text.push_str(close);
            text.push_str(trailing);
        }
        let text = text.trim_end().to_owned();
        if line.heading > 0 && !text.trim().is_empty() {
            let level = line.heading.min(6) as usize;
            out.push(format!("{} {}", "#".repeat(level), text.trim_start()));
        } else {
            out.push(text);
        }
    }
    while out.last().is_some_and(String::is_empty) {
        out.pop();
    }
    out.join("\n")
}

/// A line of plain text, escaped so Markdown shows it as it was.
fn escape_line(line: &str) -> String {
    escape_text(line, true)
}

/// Text escaped so Markdown shows it as written: emphasis and code marks,
/// and at a line's start, what would make it a heading, quote, numbered
/// list, or rule. Web addresses are left as they are, so they still work.
/// Bullets made with `-` and `+` stay lists, as people meant them.
fn escape_text(text: &str, at_line_start: bool) -> String {
    let mut out = String::with_capacity(text.len());
    if at_line_start {
        let trimmed = text.trim_start();
        let indent = &text[..text.len() - trimmed.len()];
        let rule = trimmed.len() >= 3
            && trimmed
                .chars()
                .all(|c| matches!(c, '-' | '=' | '_' | '*' | ' '));
        let numbered = trimmed
            .find(|c: char| !c.is_ascii_digit())
            .filter(|&digits| digits > 0 && digits <= 9)
            .and_then(|digits| {
                let rest = &trimmed[digits..];
                (rest.starts_with(". ") || rest.starts_with(") ") || rest == "." || rest == ")")
                    .then_some(digits)
            });
        if rule || trimmed.starts_with('#') || trimmed.starts_with('>') {
            out.push_str(indent);
            out.push('\\');
            out.push_str(&escape_inline(trimmed));
            return out;
        }
        if let Some(digits) = numbered {
            out.push_str(indent);
            out.push_str(&trimmed[..digits]);
            out.push('\\');
            out.push_str(&escape_inline(&trimmed[digits..]));
            return out;
        }
    }
    escape_inline(text)
}

/// `*`, `_`, `` ` ``, `\`, `~~`, and `<u>` escaped, except inside web
/// addresses.
fn escape_inline(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for (index, word) in text.split(' ').enumerate() {
        if index > 0 {
            out.push(' ');
        }
        if word.contains("://") {
            out.push_str(word);
            continue;
        }
        let chars: Vec<char> = word.chars().collect();
        for (at, &c) in chars.iter().enumerate() {
            let between_letters = at > 0
                && chars[at - 1].is_alphanumeric()
                && chars.get(at + 1).is_some_and(|next| next.is_alphanumeric());
            match c {
                '\\' | '*' | '`' => out.push('\\'),
                '<' if underline_tag(&chars[at..]) => out.push('\\'),
                '_' if !between_letters => out.push('\\'),
                '~' if chars.get(at + 1) == Some(&'~') || (at > 0 && chars[at - 1] == '~') => {
                    out.push('\\')
                }
                _ => {}
            }
            out.push(c);
        }
    }
    out
}

/// Whether `chars` start with `<u>` or `</u>`, which would underline.
fn underline_tag(chars: &[char]) -> bool {
    let start: String = chars
        .iter()
        .take(4)
        .collect::<String>()
        .to_ascii_lowercase();
    start.starts_with("<u>") || start.starts_with("</u>")
}

/// `text` on one line.
fn one_line(text: &str) -> String {
    text.chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect()
}

/// `text` cut to at most `max` bytes, at a character's edge for bytes or
/// characters alike: titles count characters, so a cut by bytes is never
/// longer than one by characters.
fn cut(text: &str, max: usize) -> String {
    let mut end = text.len().min(max);
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    text[..end].to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// A line as Keep writes it, with its style.
    fn p(tag: &str, text: &str, weight: u32, italic: bool, underline: bool) -> String {
        format!(
            "<{tag} dir=\"ltr\" style=\"line-height:1.38;\"><span style=\"font-size:11.0pt;\
             font-family:'Google Sans Text';color:#000000;font-weight:{weight};font-style:{};\
             text-decoration:{};white-space:pre-wrap;\">{text}</span></{tag}>",
            if italic { "italic" } else { "normal" },
            if underline { "underline" } else { "none" },
        )
    }

    #[test]
    fn turns_keeps_formatting_into_markdown() {
        let html = [
            p("p", "Plain", 400, false, false),
            p("h1", "Heading one", 400, false, false),
            p("h2", "Heading two", 400, false, false),
            p("p", "Bold", 700, false, false),
            p("p", "Italic", 400, true, false),
            p("p", "Underlined", 400, false, true),
            p("p", "All three ", 700, true, true),
            p(
                "p",
                "Tom &amp; Jerry&#39;s &quot;show&quot;",
                400,
                false,
                false,
            ),
        ]
        .concat();
        let text = "Plain\nHeading one\nHeading two\nBold\nItalic\nUnderlined\nAll three \n\
                    Tom & Jerry's \"show\"";
        assert_eq!(
            text_body(text, Some(&html)),
            "Plain\n# Heading one\n## Heading two\n**Bold**\n*Italic*\n<u>Underlined</u>\n\
             <u>***All three***</u>\nTom & Jerry's \"show\""
        );
        // Text that reads as an underline tag stays text.
        assert_eq!(escape_line("a <u>b</u> <i>"), "a \\<u>b\\</u> <i>");
    }

    #[test]
    fn keeps_a_line_break_before_the_first_line() {
        let html = format!(
            "<br />{}",
            p("p", "This note has a drawing", 400, false, false)
        );
        assert_eq!(
            text_body("\nThis note has a drawing", Some(&html)),
            "\nThis note has a drawing"
        );
    }

    #[test]
    fn marks_part_of_a_line() {
        let html = "<p><span style=\"font-weight:400\">Buy </span><span style=\"font-weight:700\">\
             milk</span><span style=\"font-weight:400\"> today</span></p>"
            .to_string();
        assert_eq!(
            text_body("Buy milk today", Some(&html)),
            "Buy **milk** today"
        );
    }

    #[test]
    fn falls_back_to_the_plain_text_when_the_html_differs() {
        let html = p("p", "Something else", 700, false, false);
        assert_eq!(text_body("What it says", Some(&html)), "What it says");
        assert_eq!(text_body("No HTML", None), "No HTML");
    }

    #[test]
    fn escapes_what_markdown_would_change() {
        assert_eq!(escape_line("# not a heading"), "\\# not a heading");
        assert_eq!(escape_line("> not a quote"), "\\> not a quote");
        assert_eq!(escape_line("2026. A year"), "2026\\. A year");
        assert_eq!(escape_line("---"), "\\---");
        assert_eq!(escape_line("5 * 3 = 15"), "5 \\* 3 = 15");
        assert_eq!(
            escape_line("snake_case and _this_"),
            "snake_case and \\_this\\_"
        );
        assert_eq!(
            escape_line("`code` and ~~not~~"),
            "\\`code\\` and \\~\\~not\\~\\~"
        );
        assert_eq!(
            escape_line("see https://a.example/x_y*z"),
            "see https://a.example/x_y*z"
        );
        assert_eq!(escape_line("- a bullet"), "- a bullet");
    }

    #[test]
    fn reads_checklists_in_keeps_order() {
        let items = json!([
            { "text": "Milk", "textHtml": p("p", "Milk", 700, false, false), "isChecked": false },
            { "text": "Bread", "isChecked": true },
            { "text": "", "isChecked": false },
        ]);
        assert_eq!(
            checklist(items.as_array().unwrap()),
            "- [ ] **Milk**\n- [x] Bread\n- [ ]\n"
        );
    }

    #[test]
    fn names_colors_as_notes_does() {
        assert_eq!(color("CERULEAN").as_deref(), Some("dark-blue"));
        assert_eq!(color("RED").as_deref(), Some("red"));
        assert_eq!(color("GRAY").as_deref(), Some("gray"));
        assert_eq!(color("DEFAULT"), None);
        assert_eq!(color("PLAID"), None);
    }

    #[test]
    fn adds_link_previews_whose_address_isnt_in_the_text() {
        let note = json!({
            "createdTimestampUsec": 1,
            "textContent": "Read https://a.example/one",
            "annotations": [
                { "source": "WEBLINK", "url": "https://a.example/one", "title": "One" },
                { "source": "WEBLINK", "url": "https://b.example/two", "title": "Two [b]" },
                { "source": "WEBLINK", "url": "javascript:alert(1)", "title": "Bad" },
                { "source": "SHEETS", "url": "https://c.example/" },
            ],
        });
        assert_eq!(
            convert(&note).body,
            "Read https://a.example/one\n\n[Two [b]](<https://b.example/two>)\n"
        );
    }

    #[test]
    fn reads_a_notes_fields() {
        let note = json!({
            "color": "CERULEAN",
            "isTrashed": true,
            "isPinned": true,
            "isArchived": false,
            "textContent": "Text",
            "title": "Plans: 2026/27?",
            "userEditedTimestampUsec": 2_000_000,
            "createdTimestampUsec": 1_000_000,
            "labels": [{ "name": "Home" }, { "name": "À faire" }],
            "tasks": [{ "id": "abc" }],
            "sharees": [{ "isOwner": true, "type": "USER", "email": "sam@example.com" }],
        });
        assert_eq!(
            convert(&note),
            KeepNote {
                title: "Plans: 2026/27?".into(),
                body: "Text".into(),
                color: Some("dark-blue".into()),
                pinned: true,
                archived: false,
                trashed: true,
                labels: vec!["Home".into(), "À faire".into()],
                created: 1_000_000,
                edited: 2_000_000,
                reminder: true,
                sharees: vec![Sharee {
                    email: "sam@example.com".into(),
                    owner: true
                }],
                missing: vec![],
                all_labels: vec![],
            }
        );
    }

    #[test]
    fn reads_keeps_list_of_labels() {
        assert_eq!(
            labels_list("\u{feff}Home\r\nÀ faire\n\nHome\n  Work  \n".as_bytes()),
            ["Home", "À faire", "Work"]
        );
    }

    #[test]
    fn reads_only_whats_there() {
        let html = "<p><span style=\"font-weight:700\">open <b>nested</b> <script>x</script>";
        let lines = read_html(html);
        assert_eq!(plain(&lines), "open nested ");
        assert_eq!(read_html("<p>a < b</p>").len(), 1);
        assert_eq!(decode("&bogus; &#xZZ; &#128512;"), "&bogus; &#xZZ; 😀");
        assert_eq!(cut("été", 2), "é");
    }
}
