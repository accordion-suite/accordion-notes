//! Transcripts of voice memos recorded in Notes (decision 33).
//!
//! A memo saved through `POST /notes/{id}/memos` waits in Notes' table
//! while transcribing is on. Notes asks core for the words, one memo at a
//! time, and adds them to the end of the note as a paragraph, as the
//! person who recorded it. A memo the note no longer links to by then gets
//! none, and a memo that fails gets none, quietly.
//!
//! An admin turns transcribing on in Settings, under Notes, by choosing a
//! model, which core downloads.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use accordion_core::db::rusqlite::{self, OptionalExtension};
use accordion_core::db::{Database, Migration};
use accordion_core::events::{Actor, Via};
use accordion_core::identity::SignedIn;
use accordion_core::live::Hint;
use accordion_core::storage::{Error, ItemKind};
use accordion_core::transcripts::{self, Model, State as ModelState, Status};
use accordion_module::{ApiError, Authenticated, Stopping};
use accordion_types::{ItemId, Role, UserId};
use axum::Json;
use axum::extract::State;
use axum::http::StatusCode;
use serde::{Deserialize, Serialize};
use tokio::sync::Notify;

use crate::NotesState;
use crate::images::{Files, dir_of};
use crate::links;
use crate::note::NoteFile;

pub(crate) const MIGRATION: Migration = Migration {
    version: 7,
    name: "Keep voice memos waiting for transcripts",
    sql: "
    -- Voice memos recorded in Notes, waiting for their words, oldest first.
    CREATE TABLE notes_transcripts (
        memo_id BLOB PRIMARY KEY NOT NULL CHECK (length(memo_id) = 16),
        note_id BLOB NOT NULL CHECK (length(note_id) = 16),
        -- Who recorded it, whose edit the transcript is.
        user_id BLOB NOT NULL CHECK (length(user_id) = 16),
        -- Its file's name, which the note's editor shows Transcribing… by.
        memo_name TEXT NOT NULL,
        asked_at TEXT NOT NULL
    ) STRICT, WITHOUT ROWID;
    CREATE INDEX notes_transcripts_in_order ON notes_transcripts (asked_at);
",
};

/// The live update kind that tells the recorder and the note's owner that
/// a memo stopped waiting without changing the note.
const HINT: &str = "notes.transcripts";

/// How long a transcript waits for the note to link to its memo, which the
/// app adds after the memo is saved.
const LINK_WAIT: Duration = Duration::from_secs(60);

/// How often it looks for the link meanwhile.
const LINK_CHECK: Duration = Duration::from_secs(2);

/// One memo waiting.
#[derive(Clone, Debug)]
struct Asked {
    memo: ItemId,
    note: ItemId,
    user: UserId,
    asked_at: jiff::Timestamp,
}

/// Memos waiting for transcripts. Cloning it is cheap.
#[derive(Clone)]
pub(crate) struct Waiting {
    db: Database,
    wake: Arc<Notify>,
}

impl Waiting {
    pub(crate) fn new(db: Database) -> Self {
        Self {
            db,
            wake: Arc::new(Notify::new()),
        }
    }

    /// Asks for a memo's transcript.
    pub(crate) async fn ask(
        &self,
        memo: ItemId,
        name: &str,
        note: ItemId,
        user: UserId,
    ) -> Result<(), Error> {
        let name = name.to_owned();
        let now = jiff::Timestamp::now().to_string();
        self.db
            .write(move |tx| {
                tx.execute(
                    "INSERT OR IGNORE INTO notes_transcripts
                         (memo_id, note_id, user_id, memo_name, asked_at)
                     VALUES (?1, ?2, ?3, ?4, ?5)",
                    rusqlite::params![memo.as_uuid(), note.as_uuid(), user.as_uuid(), name, now],
                )?;
                Ok::<_, Error>(())
            })
            .await?;
        self.wake.notify_one();
        Ok(())
    }

    /// The names of the memos waiting in each note.
    pub(crate) async fn by_note(&self) -> Result<HashMap<ItemId, Vec<String>>, Error> {
        self.db
            .read(|conn| {
                let mut statement = conn.prepare(
                    "SELECT note_id, memo_name FROM notes_transcripts ORDER BY asked_at",
                )?;
                let mut waiting: HashMap<ItemId, Vec<String>> = HashMap::new();
                let rows = statement.query_map([], |row| {
                    Ok((ItemId::from_uuid(row.get(0)?), row.get::<_, String>(1)?))
                })?;
                for row in rows {
                    let (note, name) = row?;
                    waiting.entry(note).or_default().push(name);
                }
                Ok(waiting)
            })
            .await
    }

    async fn next(&self) -> Result<Option<Asked>, Error> {
        self.db
            .read(|conn| {
                Ok(conn
                    .query_row(
                        "SELECT memo_id, note_id, user_id, asked_at FROM notes_transcripts
                         ORDER BY asked_at LIMIT 1",
                        [],
                        |row| {
                            Ok((
                                ItemId::from_uuid(row.get(0)?),
                                ItemId::from_uuid(row.get(1)?),
                                UserId::from_uuid(row.get(2)?),
                                row.get::<_, String>(3)?,
                            ))
                        },
                    )
                    .optional()?
                    .map(|(memo, note, user, asked_at)| Asked {
                        memo,
                        note,
                        user,
                        asked_at: asked_at.parse().unwrap_or_else(|_| jiff::Timestamp::now()),
                    }))
            })
            .await
    }

    async fn done(&self, memo: ItemId) -> Result<(), Error> {
        self.db
            .write(move |tx| {
                tx.execute(
                    "DELETE FROM notes_transcripts WHERE memo_id = ?1",
                    [memo.as_uuid()],
                )?;
                Ok::<_, Error>(())
            })
            .await
    }

    /// Forgets every memo waiting, such as when transcribing is turned off.
    async fn clear(&self) -> Result<Vec<Asked>, Error> {
        let mut all = Vec::new();
        while let Some(asked) = self.next().await? {
            self.done(asked.memo).await?;
            all.push(asked);
        }
        Ok(all)
    }
}

/// Transcribes waiting memos until Notes stops.
pub(crate) async fn run(state: NotesState, stopping: Stopping) {
    let mut status = state.transcripts.status();
    let mut stopping = Box::pin(stopping.wait());
    loop {
        let asked = match state.waiting.next().await {
            Ok(asked) => asked,
            Err(err) => {
                tracing::warn!(%err, "couldn't read the memos waiting for transcripts");
                None
            }
        };
        let Some(asked) = asked else {
            tokio::select! {
                () = state.waiting.wake.notified() => continue,
                () = &mut stopping => return,
            }
        };
        let outcome = tokio::select! {
            outcome = transcribe(&state, &asked) => outcome,
            () = &mut stopping => return,
        };
        match outcome {
            Outcome::Wait => {
                // The model is downloading. Memos wait for it, or for it to
                // be turned off.
                tokio::select! {
                    changed = status.changed() => {
                        if changed.is_err() {
                            return;
                        }
                    }
                    () = &mut stopping => return,
                }
            }
            Outcome::Off => match state.waiting.clear().await {
                Ok(gone) => {
                    for asked in gone {
                        tell(&state, &asked).await;
                    }
                }
                Err(err) => tracing::warn!(%err, "couldn't forget the memos waiting"),
            },
            Outcome::Done { written } => {
                if let Err(err) = state.waiting.done(asked.memo).await {
                    tracing::warn!(%err, "couldn't forget a memo that was transcribed");
                    // Not again, in a loop.
                    tokio::time::sleep(Duration::from_secs(60)).await;
                }
                if !written {
                    tell(&state, &asked).await;
                }
            }
        }
    }
}

enum Outcome {
    /// The model isn't ready yet.
    Wait,
    /// Transcribing is off.
    Off,
    /// The memo is finished with, and whether the note changed.
    Done { written: bool },
}

async fn transcribe(state: &NotesState, asked: &Asked) -> Outcome {
    let memo = match state.storage.item(asked.memo).await {
        Ok(Some(memo)) if memo.trashed_at.is_none() => memo,
        Ok(_) => return Outcome::Done { written: false },
        Err(err) => {
            tracing::warn!(%err, "couldn't find a memo to transcribe");
            return Outcome::Done { written: false };
        }
    };
    let ItemKind::File { blob, .. } = &memo.kind else {
        return Outcome::Done { written: false };
    };
    let words = match state.transcripts.transcribe(blob.hash).await {
        Ok(Some(words)) => words,
        Ok(None) => return Outcome::Done { written: false },
        Err(transcripts::Error::NotReady) => return Outcome::Wait,
        Err(transcripts::Error::Off) => return Outcome::Off,
        Err(err) => {
            tracing::warn!(%err, memo = %asked.memo, "couldn't transcribe a voice memo");
            return Outcome::Done { written: false };
        }
    };
    match add_to_note(state, asked, &words).await {
        Ok(written) => Outcome::Done { written },
        Err(err) => {
            tracing::warn!(%err, note = %asked.note, "couldn't add a transcript to its note");
            Outcome::Done { written: false }
        }
    }
}

/// Adds `words` to the end of the note, once it links to the memo, as the
/// person who recorded it. Returns whether the note changed.
async fn add_to_note(state: &NotesState, asked: &Asked, words: &str) -> Result<bool, Error> {
    loop {
        // A memo removed meanwhile gets no transcript.
        match state.storage.item(asked.memo).await? {
            Some(memo) if memo.trashed_at.is_none() => {}
            _ => return Ok(false),
        }
        if links_to_memo(state, asked).await? {
            break;
        }
        let waited = jiff::Timestamp::now().duration_since(asked.asked_at);
        if waited >= jiff::SignedDuration::try_from(LINK_WAIT).unwrap_or_default() {
            return Ok(false);
        }
        tokio::time::sleep(LINK_CHECK).await;
    }
    // What people typed in a shared note is saved first, so the change
    // merges with it, and reaches them as an edit.
    state.coediting.save_now(asked.note).await;
    let storage = state.storage.acting_as(Actor::person(asked.user, Via::Web));
    for _ in 0..3 {
        let Some(item) = state.storage.item(asked.note).await? else {
            return Ok(false);
        };
        let ItemKind::File { blob, .. } = &item.kind else {
            return Ok(false);
        };
        let mut file = NoteFile::parse(&state.store.read(asked.note).await?);
        let body = with_transcript(file.body(), words);
        file.set_body(&body);
        match storage
            .write_version(asked.note, blob.hash, file.text().as_bytes())
            .await
        {
            Ok(item) => {
                state.store.add(item.owner, &item).await?;
                return Ok(true);
            }
            // Someone saved in between, so it's added to their text.
            Err(Error::Changed) => continue,
            Err(err) => return Err(err),
        }
    }
    Ok(false)
}

/// Whether the note's text links to the memo, as Notes reads links.
async fn links_to_memo(state: &NotesState, asked: &Asked) -> Result<bool, Error> {
    let Some(note) = state.storage.item(asked.note).await? else {
        return Ok(false);
    };
    if note.trashed_at.is_some() {
        return Ok(false);
    }
    let top = crate::images::owner_place(state, note.owner).await?;
    let dir = dir_of(state, top, &note).await?;
    let files = Files::of(state, note.owner).await?;
    let text = state.store.read(asked.note).await?;
    Ok(links::links(NoteFile::parse(&text).body())
        .iter()
        .any(|link| {
            files
                .resolve(&dir, &link.target)
                .is_some_and(|file| file.id == asked.memo)
        }))
}

/// `body` with `words` as a paragraph at its end. Nothing before it
/// changes, so an editor that saved meanwhile can tell what was added.
fn with_transcript(body: &str, words: &str) -> String {
    let gap = if body.is_empty() || body.ends_with("\n\n") {
        ""
    } else if body.ends_with('\n') {
        "\n"
    } else {
        "\n\n"
    };
    format!("{body}{gap}{words}\n")
}

/// Tells the recorder and the note's owner that a memo stopped waiting.
async fn tell(state: &NotesState, asked: &Asked) {
    state.live.send(Hint::person(HINT, asked.user));
    if let Ok(Some(note)) = state.storage.item(asked.note).await
        && note.owner != asked.user
    {
        state.live.send(Hint::person(HINT, note.owner));
    }
}

/// The setting, as Settings shows it to admins.
#[derive(Serialize)]
pub(crate) struct TranscriptsView {
    /// `base`, `small`, or null when transcribing is off.
    model: Option<&'static str>,
    /// `off`, `downloading`, `ready`, or `failed`.
    state: &'static str,
    /// While downloading, how many bytes of how many.
    done: u64,
    total: u64,
    /// Why the download failed.
    problem: Option<String>,
    /// Why this server can't transcribe whatever the setting.
    unavailable: Option<String>,
    recommended: &'static str,
    models: Vec<ModelView>,
}

#[derive(Serialize)]
struct ModelView {
    id: &'static str,
    /// What it downloads, in bytes.
    size: u64,
}

impl From<&Status> for TranscriptsView {
    fn from(status: &Status) -> Self {
        let (state, done, total, problem) = match &status.state {
            ModelState::Off => ("off", 0, 0, None),
            ModelState::Downloading { done, total } => ("downloading", *done, *total, None),
            ModelState::Ready => ("ready", 0, 0, None),
            ModelState::Failed(problem) => ("failed", 0, 0, Some(problem.clone())),
        };
        Self {
            model: status.model.map(Model::id),
            state,
            done,
            total,
            problem,
            unavailable: status.unavailable.clone(),
            recommended: Model::recommended().id(),
            models: Model::ALL
                .into_iter()
                .map(|model| ModelView {
                    id: model.id(),
                    size: model.download_size(),
                })
                .collect(),
        }
    }
}

fn require_admin(me: &SignedIn) -> Result<(), ApiError> {
    if matches!(me.user.role, Role::Owner | Role::Admin) {
        return Ok(());
    }
    Err(ApiError::new(
        StatusCode::FORBIDDEN,
        "Only admins can turn transcripts on or off. Ask your admin.",
    ))
}

/// `GET /transcripts`: the setting, for admins.
pub(crate) async fn get_setting(
    State(state): State<NotesState>,
    Authenticated(me): Authenticated,
) -> Result<Json<TranscriptsView>, ApiError> {
    require_admin(&me)?;
    Ok(Json(TranscriptsView::from(
        &*state.transcripts.status().borrow(),
    )))
}

#[derive(Serialize)]
pub(crate) struct OnView {
    on: bool,
}

/// `GET /transcripts/on`: whether memos can be transcribed, for anyone
/// recording one, so the app offers it only then.
pub(crate) async fn get_on(
    State(state): State<NotesState>,
    Authenticated(_): Authenticated,
) -> Json<OnView> {
    Json(OnView {
        on: state.transcripts.is_on(),
    })
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Choice {
    /// `base`, `small`, or null to turn transcribing off.
    model: Option<String>,
}

/// `PUT /transcripts`: chooses a model, which downloads, or turns
/// transcribing off, which removes the model.
pub(crate) async fn put_setting(
    State(state): State<NotesState>,
    Authenticated(me): Authenticated,
    Json(choice): Json<Choice>,
) -> Result<Json<TranscriptsView>, ApiError> {
    require_admin(&me)?;
    let model = match choice.model.as_deref() {
        None => None,
        Some(id) => Some(Model::from_id(id).ok_or_else(|| {
            ApiError::new(
                StatusCode::BAD_REQUEST,
                "Choose the faster model or the more accurate one.",
            )
        })?),
    };
    state.transcripts.choose(model).await.map_err(|err| {
        tracing::warn!(%err, "couldn't change the transcript setting");
        ApiError::new(
            StatusCode::INTERNAL_SERVER_ERROR,
            "Accordion couldn't save the setting. Try again.",
        )
    })?;
    // Memos waiting while it was off get none.
    state.waiting.wake.notify_one();
    Ok(Json(TranscriptsView::from(
        &*state.transcripts.status().borrow(),
    )))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn adds_the_words_as_a_paragraph_at_the_end() {
        assert_eq!(
            with_transcript("![](Memo%201.webm)\n", "Buy milk."),
            "![](Memo%201.webm)\n\nBuy milk.\n"
        );
        assert_eq!(
            with_transcript("Shopping\n\n\n", "Buy milk."),
            "Shopping\n\n\nBuy milk.\n"
        );
        assert_eq!(
            with_transcript("Shopping", "Buy milk."),
            "Shopping\n\nBuy milk.\n"
        );
        assert_eq!(with_transcript("", "Buy milk."), "Buy milk.\n");
    }
}
