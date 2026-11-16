//! Links from a note to files beside it, such as its images (decision 31).
//!
//! Notes writes each image's link as its path from the top of the Notes
//! place, such as `![](Garden/Groceries%201.jpg)`, which is Obsidian's
//! "Absolute path in vault". It also reads Obsidian's own `![[name.jpg]]`,
//! and links written from the note's folder. Links to web pages are left
//! out, and so is anything inside a fenced code block.

/// A link to a file, as it's written in a note.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Link {
    /// Where the target starts and ends in the text, as written, so it can
    /// be replaced.
    pub start: usize,
    pub end: usize,
    /// The path or name it points to, with `%20` and the like decoded.
    pub target: String,
    /// Whether it shows the file, as `![](…)` and `![[…]]` do.
    pub embed: bool,
    /// Whether it's Obsidian's `[[…]]`, which isn't percent-encoded.
    pub wiki: bool,
}

/// The kinds of pictures Notes takes, by extension. Browsers show all but
/// HEIC and HEIF themselves, and Drive makes a picture of those.
pub const IMAGE_EXTENSIONS: [&str; 8] =
    ["jpg", "jpeg", "png", "gif", "webp", "avif", "heic", "heif"];

/// The extension of `name`, in lowercase, if it's a picture Notes takes.
pub fn image_extension(name: &str) -> Option<String> {
    let (_, extension) = name.rsplit_once('.')?;
    let extension = extension.to_ascii_lowercase();
    IMAGE_EXTENSIONS
        .contains(&extension.as_str())
        .then_some(extension)
}

/// Every link to a file in `text`, in order.
pub fn links(text: &str) -> Vec<Link> {
    let mut found = Vec::new();
    let mut fence: Option<&str> = None;
    let mut at = 0;
    for line in text.split_inclusive('\n') {
        let start = at;
        at += line.len();
        let trimmed = line.trim_start();
        if let Some(open) = fence {
            if trimmed.starts_with(open) {
                fence = None;
            }
            continue;
        }
        if trimmed.starts_with("```") {
            fence = Some("```");
            continue;
        }
        if trimmed.starts_with("~~~") {
            fence = Some("~~~");
            continue;
        }
        links_in_line(line, start, &mut found);
    }
    found
}

fn links_in_line(line: &str, offset: usize, found: &mut Vec<Link>) {
    let bytes = line.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'\\' => i += 2,
            b'`' => {
                // Code spans hold text, not links.
                let run = bytes[i..].iter().take_while(|&&b| b == b'`').count();
                let fence = &line[i..i + run];
                i = match line[i + run..].find(fence) {
                    Some(end) => i + run + end + run,
                    None => i + run,
                };
            }
            b'[' if bytes.get(i + 1) == Some(&b'[') => {
                let embed = i > 0 && bytes[i - 1] == b'!';
                match wiki_at(line, i + 2) {
                    Some((start, end, next)) => {
                        let target = line[start..end].trim();
                        if is_local(target) {
                            let lead = line[start..end].len() - line[start..end].trim_start().len();
                            found.push(Link {
                                start: offset + start + lead,
                                end: offset + start + lead + target.len(),
                                target: target.to_owned(),
                                embed,
                                wiki: true,
                            });
                        }
                        i = next;
                    }
                    None => i += 2,
                }
            }
            b'[' => {
                let embed = i > 0 && bytes[i - 1] == b'!';
                match markdown_at(line, i) {
                    Some((start, end, raw, next)) => {
                        if let Some(target) = decode(raw).filter(|target| is_local(target)) {
                            found.push(Link {
                                start: offset + start,
                                end: offset + end,
                                target,
                                embed,
                                wiki: false,
                            });
                        }
                        i = next;
                    }
                    None => i += 1,
                }
            }
            _ => i += 1,
        }
    }
}

/// The target of `[[target|label]]`, whose text starts at `from`: where
/// it starts and ends, and where the link ends.
fn wiki_at(line: &str, from: usize) -> Option<(usize, usize, usize)> {
    let close = line[from..].find("]]")? + from;
    let inner = &line[from..close];
    if inner.contains('[') {
        return None;
    }
    let length = inner.find(['|', '#', '^']).unwrap_or(inner.len());
    Some((from, from + length, close + 2))
}

/// The destination of `[label](destination)`, with `[` at `open`: where
/// it starts and ends, as written, the text there without angle brackets,
/// and where the link ends.
fn markdown_at(line: &str, open: usize) -> Option<(usize, usize, &str, usize)> {
    let bytes = line.as_bytes();
    let mut depth = 0;
    let mut i = open;
    let close = loop {
        match bytes.get(i)? {
            b'\\' => i += 1,
            b'[' => depth += 1,
            b']' => {
                depth -= 1;
                if depth == 0 {
                    break i;
                }
            }
            _ => {}
        }
        i += 1;
    };
    if bytes.get(close + 1) != Some(&b'(') {
        return None;
    }
    let start = close + 2;
    if bytes.get(start) == Some(&b'<') {
        let end = line[start..].find('>')? + start;
        let after = line[end + 1..].find(')')? + end + 1;
        return Some((start, end + 1, &line[start + 1..end], after + 1));
    }
    let mut depth = 0;
    let mut i = start;
    let end = loop {
        match bytes.get(i)? {
            b'\\' => i += 1,
            b'(' => depth += 1,
            b')' if depth == 0 => break i,
            b')' => depth -= 1,
            _ => {}
        }
        i += 1;
    };
    // A title, as in `(photo.jpg "A photo")`, isn't part of the target.
    let written = &line[start..end];
    let target = match written.find(" \"").or_else(|| written.find(" '")) {
        Some(title) => &written[..title],
        None => written,
    };
    let target_end = start + target.trim_end().len();
    let lead = target.len() - target.trim_start().len();
    Some((
        start + lead,
        target_end,
        line[start + lead..target_end].as_ref(),
        end + 1,
    ))
}

/// Whether a link points at a file rather than a web page, an email
/// address, or a place in the same note.
fn is_local(target: &str) -> bool {
    !target.is_empty()
        && !target.starts_with('#')
        && !target.starts_with('/')
        && !target.contains("://")
        && !target.to_ascii_lowercase().starts_with("mailto:")
        && !target.to_ascii_lowercase().starts_with("data:")
}

/// `%20` and the like, decoded. `None` if the result isn't text.
fn decode(raw: &str) -> Option<String> {
    let bytes = raw.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%'
            && let Some(byte) = raw
                .get(i + 1..i + 3)
                .and_then(|hex| u8::from_str_radix(hex, 16).ok())
        {
            out.push(byte);
            i += 3;
            continue;
        }
        if bytes[i] == b'\\' && i + 1 < bytes.len() && bytes[i + 1].is_ascii_punctuation() {
            i += 1;
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8(out).ok()
}

/// A path written so a Markdown link holds it whole: the characters that
/// would end the link or change its meaning are percent-encoded, as
/// Obsidian writes them.
pub fn encode(path: &str) -> String {
    let mut out = String::with_capacity(path.len());
    for char in path.chars() {
        match char {
            ' ' => out.push_str("%20"),
            '%' => out.push_str("%25"),
            '(' => out.push_str("%28"),
            ')' => out.push_str("%29"),
            '<' => out.push_str("%3C"),
            '>' => out.push_str("%3E"),
            '[' => out.push_str("%5B"),
            ']' => out.push_str("%5D"),
            '#' => out.push_str("%23"),
            '^' => out.push_str("%5E"),
            '|' => out.push_str("%7C"),
            '\\' => out.push_str("%5C"),
            other => out.push(other),
        }
    }
    out
}

/// The line that shows an image, for the top of a note.
pub fn image_line(path: &str) -> String {
    format!("![]({})", encode(path))
}

/// `text` with every link to `old` pointing at `new` instead, or `None` if
/// nothing links to `old`.
pub fn rewrite(text: &str, old: &str, new: &str) -> Option<String> {
    let found: Vec<Link> = links(text)
        .into_iter()
        .filter(|link| link.target == old)
        .collect();
    if found.is_empty() {
        return None;
    }
    let mut out = String::with_capacity(text.len() + 16);
    let mut at = 0;
    for link in found {
        out.push_str(&text[at..link.start]);
        if link.wiki {
            out.push_str(new);
        } else {
            out.push_str(&encode(new));
        }
        at = link.end;
    }
    out.push_str(&text[at..]);
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn targets(text: &str) -> Vec<(String, bool, bool)> {
        links(text)
            .into_iter()
            .map(|link| (link.target, link.embed, link.wiki))
            .collect()
    }

    #[test]
    fn reads_markdown_and_obsidian_links() {
        let text = "![](Garden/Groceries%201.jpg)\n\
                    See [the plan](plan.pdf \"Plan\") and ![[photo.png|200]].\n\
                    [[Other note#Heading]] and <https://example.com> [web](https://example.com/a.jpg)\n\
                    ![](<With space.jpg>)\n";
        assert_eq!(
            targets(text),
            vec![
                ("Garden/Groceries 1.jpg".into(), true, false),
                ("plan.pdf".into(), false, false),
                ("photo.png".into(), true, true),
                ("Other note".into(), false, true),
                ("With space.jpg".into(), true, false),
            ]
        );
    }

    #[test]
    fn leaves_out_code() {
        let text = "```\n![](inside.jpg)\n```\n`![](span.jpg)` ![](after.jpg)\n";
        assert_eq!(targets(text), vec![("after.jpg".into(), true, false)]);
    }

    #[test]
    fn encodes_what_would_break_a_link() {
        assert_eq!(encode("A (1)/b c#2.jpg"), "A%20%281%29/b%20c%232.jpg");
        assert_eq!(
            links(&image_line("A (1)/b c#2.jpg"))[0].target,
            "A (1)/b c#2.jpg"
        );
    }

    #[test]
    fn rewrites_only_the_moved_file() {
        let text = "![](a%20b.jpg)\n![[a b.jpg]]\n![](c.jpg)\n[a](a%20b.jpg \"t\")\n";
        assert_eq!(
            rewrite(text, "a b.jpg", "Garden/a b.jpg").unwrap(),
            "![](Garden/a%20b.jpg)\n![[Garden/a b.jpg]]\n![](c.jpg)\n[a](Garden/a%20b.jpg \"t\")\n"
        );
        assert_eq!(rewrite(text, "d.jpg", "e.jpg"), None);
    }

    #[test]
    fn knows_pictures() {
        assert_eq!(image_extension("A.JPG").as_deref(), Some("jpg"));
        assert_eq!(image_extension("a.svg"), None);
        assert_eq!(image_extension("noextension"), None);
    }
}
