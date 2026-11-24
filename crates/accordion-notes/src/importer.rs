//! Storing Keep's notes from Google Takeout (decision 34), on the server.
//! The sandbox read each note into a [`KeepNote`]; this checks it again,
//! since it came from the archive, and saves it as a note in the person's
//! Notes place, with its pictures and voice recordings beside it.

use std::collections::HashSet;
use std::io::Read;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use accordion_core::storage::{Dates, Error};
use accordion_core::takeout::imports::BoxFuture;
use accordion_core::takeout::{Importer, Importing, ItemError, Unpacked, UnpackedFile, escape};
use accordion_types::ItemId;

use crate::NotesState;
use crate::keep::{self, KeepNote, NotANote};
use crate::links::{self, image_extension, sound_extension};
use crate::note::{self, Details, NoteFile};

/// The report's headings for what only Keep has.
const REMINDERS: &str = "Reminders to set again";
const SHARED: &str = "Notes that were shared";
const MISSING: &str = "Attachments not in the archives";
const VOICE: &str = "Voice recordings";

/// The largest attachment Notes takes, as for pictures added in Notes.
const MAX_ATTACHMENT: u64 = crate::images::MAX_IMAGE as u64;

/// The longest label Notes takes, in characters.
const LABEL_CHARS: usize = 50;

pub(crate) struct KeepImporter {
    state: NotesState,
}

impl KeepImporter {
    pub(crate) fn new(state: NotesState) -> Self {
        Self { state }
    }
}

impl Importer for KeepImporter {
    fn product(&self) -> &'static str {
        keep::PRODUCT
    }

    fn app(&self) -> &'static str {
        "Notes"
    }

    fn brought(&self, count: u64) -> String {
        let notes = if count == 1 { "note" } else { "notes" };
        format!("{count} {notes} from Keep, in Notes")
    }

    fn label(&self, item: &Unpacked) -> String {
        if let Ok(not_a_note) = serde_json::from_value::<NotANote>(item.fields.clone()) {
            return escape(&not_a_note.name);
        }
        match serde_json::from_value::<KeepNote>(item.fields.clone()) {
            Ok(note) => label(&note),
            Err(_) => escape(&item.key),
        }
    }

    fn store<'a>(
        &'a self,
        item: &'a Unpacked,
        importing: &'a Importing,
    ) -> BoxFuture<'a, Result<Option<ItemId>, ItemError>> {
        Box::pin(self.store_note(item, importing))
    }
}

/// A note's name in the report, in Markdown.
fn label(note: &KeepNote) -> String {
    let title = match note.title.trim() {
        "" => note::derived_title(&note.body),
        title => title.to_owned(),
    };
    format!("**{}**", escape(&title))
}

impl KeepImporter {
    async fn store_note(
        &self,
        item: &Unpacked,
        importing: &Importing,
    ) -> Result<Option<ItemId>, ItemError> {
        if let Ok(not_a_note) = serde_json::from_value::<NotANote>(item.fields.clone()) {
            return Err(ItemError::NotConverted(escape(&not_a_note.not_a_note)));
        }
        let note = serde_json::from_value::<KeepNote>(item.fields.clone())
            .map(checked)
            .map_err(|_| ItemError::NotConverted("Accordion couldn't read this note.".into()))?;
        let label = label(&note);
        let person = importing.person();
        let top = self
            .state
            .store
            .folder(person)
            .await
            .map_err(ItemError::from)?;
        let name = note::file_name(&note.title, &note.body);
        let title = note::title_of(&name).to_owned();
        if !note.title.trim().is_empty()
            && title.chars().count() < note.title.trim().chars().count()
        {
            importing.shortened(note.title.trim(), &title);
        }
        let dates = Dates {
            created: time(note.created),
            modified: time(note.edited.max(note.created)),
        };

        // Pictures and recordings first, named after the note, so its text
        // can show them at its top, as a picture added in Notes is.
        let attachments: Vec<&UnpackedFile> = item
            .files
            .iter()
            .filter(|file| file.role == "attachment")
            .collect();
        let mut taken = HashSet::new();
        if !attachments.is_empty() {
            taken = importing
                .storage()
                .children(top)
                .await?
                .into_iter()
                .filter_map(|child| Some(child.name.rsplit_once('.')?.0.to_lowercase()))
                .collect();
        }
        let mut media = Vec::new();
        for file in attachments {
            if let Some(saved) = self
                .attachment(importing, top, &title, &label, file, dates, &mut taken)
                .await?
            {
                media.push(saved);
            }
        }
        for missing in &note.missing {
            importing.report(MISSING, format!("{label}: {}", escape(missing)));
        }

        let mut body = String::new();
        for (_, name) in &media {
            body.push_str(&links::image_line(name));
            body.push('\n');
        }
        if !media.is_empty() && !note.body.is_empty() {
            body.push('\n');
        }
        body.push_str(&note.body);
        let mut file = NoteFile::new(&body);
        file.set_details(&Details {
            color: note.color.clone(),
            pinned: note.pinned,
            archived: note.archived,
            tags: note.labels.clone(),
        });
        let saved = importing
            .create_file(top, &name, file.text().as_bytes(), Some(dates))
            .await?;

        if note.trashed {
            // To the trash with its pictures, which come back with it.
            let mut trashed = Vec::new();
            for (id, _) in &media {
                match importing.storage().delete(*id).await {
                    Ok(_) => trashed.push(*id),
                    Err(Error::InTrash | Error::NotFound) => {}
                    Err(err) => return Err(err.into()),
                }
            }
            self.state
                .store
                .trashed_with(saved.id, trashed)
                .await
                .map_err(ItemError::from)?;
            importing.storage().delete(saved.id).await?;
        } else {
            self.state
                .store
                .add(person, &saved)
                .await
                .map_err(ItemError::from)?;
        }

        if note.reminder {
            importing.report(REMINDERS, label.clone());
        }
        if !note.sharees.is_empty() {
            let mut with = Vec::new();
            let mut owner = None;
            for sharee in &note.sharees {
                if sharee.owner {
                    owner = Some(escape(&sharee.email));
                } else {
                    with.push(escape(&sharee.email));
                }
            }
            let mut line = label.clone();
            if !with.is_empty() {
                line.push_str(&format!(": shared with {}", with.join(", ")));
            }
            if let Some(owner) = owner {
                line.push_str(&format!(
                    "{} owned by {owner}",
                    if with.is_empty() { ":" } else { ";" }
                ));
            }
            importing.report(SHARED, line);
        }
        Ok(Some(saved.id))
    }

    /// Saves one of the note's attachments beside it, as Notes shows it,
    /// and returns it with its name. One Notes can't show goes to
    /// **Unconverted**.
    #[allow(clippy::too_many_arguments)]
    async fn attachment(
        &self,
        importing: &Importing,
        top: ItemId,
        title: &str,
        label: &str,
        file: &UnpackedFile,
        dates: Dates,
        taken: &mut HashSet<String>,
    ) -> Result<Option<(ItemId, String)>, ItemError> {
        let archived = file.path.rsplit('/').next().unwrap_or(&file.path);
        if file.size > MAX_ATTACHMENT {
            importing
                .unconverted(file, &format!("it's larger than 50 MB, from {label}"))
                .await?;
            return Ok(None);
        }
        let head = read_head(&file.file).map_err(|err| ItemError::Stop(err.to_string()))?;
        let mut name_for = |extension: &str| {
            let stem = (1..)
                .map(|number| format!("{title} {number}"))
                .find(|stem| !taken.contains(&stem.to_lowercase()))
                .expect("some number is free");
            taken.insert(stem.to_lowercase());
            format!("{stem}.{extension}")
        };

        // Keep's recordings are AMR, which browsers can't play, though
        // they're named `.3gp`.
        if head.starts_with(b"#!AMR") {
            let contents = tokio::fs::read(&file.file)
                .await
                .map_err(|err| ItemError::Stop(err.to_string()))?;
            return match self.state.thumbnails.voice_to_opus(&contents).await {
                Ok(opus) => {
                    let saved = importing
                        .create_file(top, &name_for("ogg"), &opus, Some(dates))
                        .await?;
                    importing.keep(file).await?;
                    importing.report(
                        VOICE,
                        format!(
                            "{label}: converted so browsers can play it. The original, `{}`, is \
                             in **Unconverted**.",
                            escape(archived)
                        ),
                    );
                    Ok(Some((saved.id, saved.name)))
                }
                Err(why) => {
                    importing
                        .unconverted(
                            file,
                            &format!(
                                "a voice recording from {label} that Accordion couldn't \
                                 convert: {}",
                                escape(&why)
                            ),
                        )
                        .await?;
                    Ok(None)
                }
            };
        }

        let extension = image_extension(archived)
            .filter(|_| is_picture(&head))
            .or_else(|| sound_extension(archived));
        let Some(extension) = extension else {
            importing
                .unconverted(
                    file,
                    &format!("Notes can't show this attachment of {label}"),
                )
                .await?;
            return Ok(None);
        };
        let saved = importing
            .create_file_from(top, &name_for(&extension), file, Some(dates))
            .await?;
        Ok(Some((saved.id, saved.name)))
    }
}

/// The note as Notes takes it, whatever the sandbox said: sizes capped,
/// and only Notes' colors and labels.
fn checked(mut note: KeepNote) -> KeepNote {
    note.title = note
        .title
        .chars()
        .take(keep::MAX_TITLE)
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect();
    if note.body.len() > keep::MAX_BODY {
        let mut end = keep::MAX_BODY;
        while !note.body.is_char_boundary(end) {
            end -= 1;
        }
        note.body.truncate(end);
    }
    note.color = note
        .color
        .filter(|color| note::COLORS.contains(&color.as_str()));
    let mut labels = Vec::new();
    for label in note.labels.iter().take(keep::MAX_LIST) {
        let label: String = label
            .trim()
            .trim_start_matches('#')
            .trim()
            .chars()
            .take(LABEL_CHARS)
            .map(|c| if c.is_control() { ' ' } else { c })
            .collect();
        if !label.is_empty() && !labels.contains(&label) {
            labels.push(label);
        }
    }
    note.labels = labels;
    note.sharees.truncate(keep::MAX_LIST);
    note.missing.truncate(keep::MAX_LIST);
    note
}

/// A time in microseconds since 1970.
fn time(usec: i64) -> SystemTime {
    UNIX_EPOCH + Duration::from_micros(usec.max(0).unsigned_abs())
}

/// The first bytes of a file, to tell what it is.
fn read_head(path: &std::path::Path) -> std::io::Result<Vec<u8>> {
    let mut head = Vec::with_capacity(16);
    std::fs::File::open(path)?.take(16).read_to_end(&mut head)?;
    Ok(head)
}

/// Whether `head` starts a picture Notes shows: JPEG, PNG, GIF, WebP, or
/// HEIC and AVIF in their ISO boxes.
fn is_picture(head: &[u8]) -> bool {
    head.starts_with(&[0xff, 0xd8, 0xff])
        || head.starts_with(b"\x89PNG")
        || head.starts_with(b"GIF8")
        || (head.starts_with(b"RIFF") && head.get(8..12) == Some(b"WEBP"))
        || head.get(4..8) == Some(b"ftyp")
}
