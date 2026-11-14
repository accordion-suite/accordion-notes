//! Notes' JSON API, at `/api/notes`, for the web app and later the phone
//! app. Everything here acts on the signed-in person's own notes. Notes
//! shared with them come with co-editing (decision 31's second step).

use std::collections::HashSet;

use accordion_core::identity::SignedIn;
use accordion_core::storage::{Error, Item, ItemKind, Storage};
use accordion_module::{ApiError, Authenticated};
use accordion_types::{ItemId, UserId};
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::routing::{get, post, put};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};

use crate::NotesState;
use crate::note::{self, Details, NoteFile};
use crate::store::{Indexed, shorten};

/// How much of each note the list sends, for its card.
const CARD_CHARS: usize = 1500;

pub(crate) fn routes(state: NotesState) -> Router {
    Router::new()
        .route("/notes", get(list).post(create))
        .route("/notes/{id}", get(open).put(update).delete(trash))
        .route("/trash", get(trashed).delete(empty_trash))
        .route("/trash/{id}", axum::routing::delete(delete_forever))
        .route("/trash/{id}/restore", post(restore))
        .route("/labels", get(labels).post(add_label))
        .route("/labels/{name}", put(rename_label).delete(remove_label))
        .with_state(state)
}

/// A note, as the web app shows it.
#[derive(Serialize)]
struct NoteView {
    id: ItemId,
    title: String,
    /// Whether someone gave the note its title. Otherwise the title is its
    /// first words, which the card doesn't repeat.
    titled: bool,
    /// All of the text when opened, the start of it in lists.
    body: String,
    color: Option<String>,
    pinned: bool,
    archived: bool,
    tags: Vec<String>,
    /// The folder it's in, or null at the top of the Notes place.
    folder: Option<ItemId>,
    /// The contents' hash, to send back with changes to the text.
    hash: String,
    created_at: String,
    modified_at: String,
}

impl NoteView {
    fn new(indexed: Indexed, body: String, top: ItemId) -> Self {
        Self {
            id: indexed.id,
            folder: indexed.folder.filter(|folder| *folder != top),
            title: note::title_of(&indexed.name).to_owned(),
            titled: note::has_title(&indexed.name, &body),
            body,
            color: indexed.details.color,
            pinned: indexed.details.pinned,
            archived: indexed.details.archived,
            tags: indexed.details.tags,
            hash: indexed.hash,
            created_at: indexed.created_at,
            modified_at: indexed.modified_at,
        }
    }
}

#[derive(Serialize)]
struct NotesView {
    notes: Vec<NoteView>,
    labels: Vec<String>,
    /// Every folder in the Notes place, for the sidebar.
    folders: Vec<FolderView>,
    /// The Notes place's top folder, which the web app moves notes and
    /// folders back into, and makes folders in, through Drive's API, so
    /// Drive asks before a move shares anything (decision 32).
    top: ItemId,
}

/// A folder in the Notes place.
#[derive(Serialize)]
struct FolderView {
    id: ItemId,
    name: String,
    /// The folder it's in, or null at the top of the Notes place.
    parent: Option<ItemId>,
}

#[derive(Deserialize)]
struct ListQuery {
    /// Words that all have to be in the title, labels, or text.
    q: Option<String>,
    /// Only notes in this folder, and in the folders inside it.
    folder: Option<ItemId>,
}

/// Storage acting as the signed-in person, so their changes are recorded
/// as theirs.
fn by(state: &NotesState, me: &SignedIn) -> Storage {
    state.storage.acting_as(accordion_module::auth::actor(me))
}

/// The person's Notes folder, with their index brought up to date.
async fn ready(state: &NotesState, me: &SignedIn) -> Result<ItemId, ApiError> {
    let folder = state.store.folder(me.user.id).await?;
    state.store.follow(me.user.id, folder).await?;
    Ok(folder)
}

/// One of the person's notes, or "not found", so nobody learns what other
/// people have.
async fn own_note(
    state: &NotesState,
    user: UserId,
    folder: ItemId,
    id: ItemId,
) -> Result<Item, ApiError> {
    let item = state
        .storage
        .item(id)
        .await?
        .ok_or_else(ApiError::not_found)?;
    if !state.store.is_note(user, folder, &item).await? {
        return Err(ApiError::not_found());
    }
    Ok(item)
}

/// The folders in the Notes place, and the ones inside `folder`, counting
/// itself, when there is one. A folder outside the place is "not found".
async fn folders(
    state: &NotesState,
    top: ItemId,
    folder: Option<ItemId>,
) -> Result<(Vec<Item>, Option<HashSet<ItemId>>), ApiError> {
    let all = state.store.folders(top).await?;
    let Some(folder) = folder else {
        return Ok((all, None));
    };
    if folder != top && !all.iter().any(|item| item.id == folder) {
        return Err(ApiError::not_found());
    }
    let mut within = HashSet::from([folder]);
    // Parents come before what's inside them, so one pass finds them all.
    for item in &all {
        if item.parent.is_some_and(|parent| within.contains(&parent)) {
            within.insert(item.id);
        }
    }
    Ok((all, Some(within)))
}

/// `GET /notes`: every note, newest first, with the labels and folders.
async fn list(
    State(state): State<NotesState>,
    Authenticated(me): Authenticated,
    Query(query): Query<ListQuery>,
) -> Result<Json<NotesView>, ApiError> {
    let top = ready(&state, &me).await?;
    let (all_folders, within) = folders(&state, top, query.folder).await?;
    let words: Vec<String> = query
        .q
        .unwrap_or_default()
        .split_whitespace()
        .map(str::to_lowercase)
        .collect();
    let notes = state
        .store
        .all(me.user.id)
        .await?
        .into_iter()
        .filter(|indexed| match &within {
            Some(within) => indexed
                .folder
                .is_some_and(|folder| within.contains(&folder)),
            None => true,
        })
        .filter(|indexed| {
            if words.is_empty() {
                return true;
            }
            let text = format!(
                "{}\n{}\n{}",
                note::title_of(&indexed.name),
                indexed.details.tags.join("\n"),
                indexed.body
            )
            .to_lowercase();
            words.iter().all(|word| text.contains(word))
        })
        .map(|indexed| {
            let body = shorten(&indexed.body, CARD_CHARS);
            // Whether it has a title depends on the whole first line, which
            // the index has.
            let titled = note::has_title(&indexed.name, &indexed.body);
            NoteView {
                titled,
                ..NoteView::new(indexed, body, top)
            }
        })
        .collect();
    let folders = all_folders
        .into_iter()
        .map(|item| FolderView {
            id: item.id,
            name: item.name,
            parent: item.parent.filter(|parent| *parent != top),
        })
        .collect();
    Ok(Json(NotesView {
        notes,
        labels: state.store.labels(me.user.id).await?,
        folders,
        top,
    }))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct NewNote {
    #[serde(default)]
    title: String,
    #[serde(default)]
    body: String,
    color: Option<String>,
    #[serde(default)]
    pinned: bool,
    #[serde(default)]
    archived: bool,
    #[serde(default)]
    tags: Vec<String>,
    /// A folder in the Notes place to save it in, rather than the top.
    folder: Option<ItemId>,
}

/// `POST /notes`: takes a note.
async fn create(
    State(state): State<NotesState>,
    Authenticated(me): Authenticated,
    Json(request): Json<NewNote>,
) -> Result<(StatusCode, Json<NoteView>), ApiError> {
    let top = ready(&state, &me).await?;
    let folder = match request.folder {
        Some(folder) if folder != top => {
            folders(&state, top, Some(folder)).await?;
            folder
        }
        _ => top,
    };
    let mut file = NoteFile::new(&request.body);
    file.set_details(&Details {
        color: check_color(request.color)?,
        pinned: request.pinned,
        archived: request.archived,
        tags: check_tags(request.tags)?,
    });
    let text = file.text();
    let name = note::file_name(&request.title, &request.body);
    let item = numbered(&name, |name| {
        let storage = by(&state, &me);
        let bytes = text.clone().into_bytes();
        async move { storage.create_file(folder, &name, bytes.as_slice()).await }
    })
    .await?;
    let indexed = state.store.add(me.user.id, &item).await?;
    let body = file.body().to_owned();
    Ok((StatusCode::CREATED, Json(NoteView::new(indexed, body, top))))
}

/// `GET /notes/{id}`: a note with all of its text.
async fn open(
    State(state): State<NotesState>,
    Authenticated(me): Authenticated,
    Path(id): Path<ItemId>,
) -> Result<Json<NoteView>, ApiError> {
    let folder = ready(&state, &me).await?;
    let item = own_note(&state, me.user.id, folder, id).await?;
    let indexed = state.store.add(me.user.id, &item).await?;
    let file = NoteFile::parse(&state.store.read(id).await?);
    Ok(Json(NoteView::new(indexed, file.body().to_owned(), folder)))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct NoteChange {
    /// The hash the change was made to, needed with a new text, so it
    /// isn't saved over someone else's change.
    base: Option<String>,
    /// An empty title names the note after its first words.
    title: Option<String>,
    body: Option<String>,
    /// An empty color takes the color away.
    color: Option<String>,
    pinned: Option<bool>,
    archived: Option<bool>,
    tags: Option<Vec<String>>,
}

/// `PUT /notes/{id}`: changes what's given and keeps the rest.
async fn update(
    State(state): State<NotesState>,
    Authenticated(me): Authenticated,
    Path(id): Path<ItemId>,
    Json(change): Json<NoteChange>,
) -> Result<Json<NoteView>, ApiError> {
    let folder = ready(&state, &me).await?;
    let item = own_note(&state, me.user.id, folder, id).await?;
    let item = change_note(&state, &me, item, change).await?;
    let indexed = state.store.add(me.user.id, &item).await?;
    let file = NoteFile::parse(&state.store.read(id).await?);
    Ok(Json(NoteView::new(indexed, file.body().to_owned(), folder)))
}

/// Writes a change to a note's file and name, and returns the note as it
/// is after.
async fn change_note(
    state: &NotesState,
    me: &SignedIn,
    item: Item,
    change: NoteChange,
) -> Result<Item, ApiError> {
    let ItemKind::File { blob, .. } = &item.kind else {
        return Err(ApiError::not_found());
    };
    let hash = blob.hash;
    if change.body.is_some() && change.base.as_deref() != Some(hash.to_hex().as_str()) {
        return Err(Error::Changed.into());
    }
    let before = state.store.read(item.id).await?;
    let mut file = NoteFile::parse(&before);
    let old_body = file.body().to_owned();
    if let Some(body) = &change.body {
        file.set_body(body);
    }
    let mut details = file.details();
    if let Some(color) = change.color {
        details.color = check_color(Some(color))?;
    }
    if let Some(pinned) = change.pinned {
        details.pinned = pinned;
    }
    if let Some(archived) = change.archived {
        details.archived = archived;
    }
    if let Some(tags) = change.tags {
        details.tags = check_tags(tags)?;
    }
    file.set_details(&details);

    let storage = by(state, me);
    let mut item = item.clone();
    let text = file.text();
    if text != before {
        item = storage
            .write_version(item.id, hash, text.as_bytes())
            .await?;
    }

    // A note without a title follows its first words.
    let wanted = match &change.title {
        Some(title) => Some(note::file_name(title, file.body())),
        None if change.body.is_some() && !note::has_title(&item.name, &old_body) => {
            Some(note::file_name("", file.body()))
        }
        None => None,
    };
    if let Some(wanted) = wanted
        && unnumbered(note::title_of(&item.name)) != note::title_of(&wanted)
    {
        let parent = item.parent.ok_or(Error::NotAFile)?;
        let id = item.id;
        item = numbered(&wanted, |name| {
            let storage = storage.clone();
            async move { storage.move_item(id, parent, &name).await }
        })
        .await?;
    }
    Ok(item)
}

/// A title without the number added to make its name unique.
fn unnumbered(title: &str) -> &str {
    match title.rsplit_once(" (") {
        Some((base, number))
            if number
                .strip_suffix(')')
                .is_some_and(|n| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit())) =>
        {
            base
        }
        _ => title,
    }
}

/// Tries `name`, then `name (2)`, `name (3)`, and so on, while the name is
/// taken.
async fn numbered<F, Fut>(name: &str, mut attempt: F) -> Result<Item, Error>
where
    F: FnMut(String) -> Fut,
    Fut: Future<Output = Result<Item, Error>>,
{
    let title = note::title_of(name);
    let extension = &name[title.len()..];
    for number in 1..=100 {
        let candidate = match number {
            1 => name.to_owned(),
            n => format!("{title} ({n}){extension}"),
        };
        match attempt(candidate).await {
            Err(Error::NameTaken { .. }) => continue,
            result => return result,
        }
    }
    Err(Error::NameTaken { folder: false })
}

fn check_color(color: Option<String>) -> Result<Option<String>, ApiError> {
    match color {
        None => Ok(None),
        Some(color) if color.is_empty() => Ok(None),
        Some(color) if note::COLORS.contains(&color.as_str()) => Ok(Some(color)),
        Some(_) => Err(ApiError::new(
            StatusCode::BAD_REQUEST,
            "Choose one of the colors Notes offers.",
        )),
    }
}

/// The longest label, in characters.
const LABEL_CHARS: usize = 50;

fn check_tags(tags: Vec<String>) -> Result<Vec<String>, ApiError> {
    tags.into_iter().map(|tag| check_label(&tag)).collect()
}

fn check_label(label: &str) -> Result<String, ApiError> {
    let label = label.trim().trim_start_matches('#').trim();
    if label.is_empty() || label.chars().count() > LABEL_CHARS || label.contains(['\n', '\r']) {
        return Err(ApiError::new(
            StatusCode::BAD_REQUEST,
            format!("A label can be 1 to {LABEL_CHARS} characters long, on one line."),
        ));
    }
    Ok(label.to_owned())
}

/// `DELETE /notes/{id}`: moves a note to the trash, which is Drive's.
async fn trash(
    State(state): State<NotesState>,
    Authenticated(me): Authenticated,
    Path(id): Path<ItemId>,
) -> Result<StatusCode, ApiError> {
    let folder = ready(&state, &me).await?;
    own_note(&state, me.user.id, folder, id).await?;
    by(&state, &me).delete(id).await?;
    state.store.remove(id).await?;
    Ok(StatusCode::NO_CONTENT)
}

/// A note in the trash.
#[derive(Serialize)]
struct TrashedView {
    id: ItemId,
    title: String,
    titled: bool,
    body: String,
    color: Option<String>,
    trashed_at: String,
}

#[derive(Serialize)]
struct TrashView {
    notes: Vec<TrashedView>,
}

/// The person's notes in the trash: files that were in their Notes folder.
async fn trashed_notes(
    state: &NotesState,
    user: UserId,
    folder: ItemId,
) -> Result<Vec<Item>, ApiError> {
    let mut notes = Vec::new();
    for item in state.storage.trash(user).await? {
        let Some(parent) = item.parent else { continue };
        if matches!(item.kind, ItemKind::File { .. })
            && note::is_note(&item.name)
            && state.storage.path_within(folder, parent).await?.is_some()
        {
            notes.push(item);
        }
    }
    Ok(notes)
}

/// `GET /trash`: notes in the trash, newest first.
async fn trashed(
    State(state): State<NotesState>,
    Authenticated(me): Authenticated,
) -> Result<Json<TrashView>, ApiError> {
    let folder = ready(&state, &me).await?;
    let mut notes = Vec::new();
    for item in trashed_notes(&state, me.user.id, folder).await? {
        let file = NoteFile::parse(&state.store.read(item.id).await?);
        notes.push(TrashedView {
            id: item.id,
            title: note::title_of(&item.name).to_owned(),
            titled: note::has_title(&item.name, file.body()),
            body: shorten(file.body(), CARD_CHARS),
            color: file.details().color,
            trashed_at: item.trashed_at.unwrap_or_default(),
        });
    }
    Ok(Json(TrashView { notes }))
}

/// One of the person's notes in the trash, or "not found".
async fn trashed_note(
    state: &NotesState,
    user: UserId,
    folder: ItemId,
    id: ItemId,
) -> Result<Item, ApiError> {
    trashed_notes(state, user, folder)
        .await?
        .into_iter()
        .find(|item| item.id == id)
        .ok_or_else(ApiError::not_found)
}

/// `POST /trash/{id}/restore`: takes a note out of the trash.
async fn restore(
    State(state): State<NotesState>,
    Authenticated(me): Authenticated,
    Path(id): Path<ItemId>,
) -> Result<Json<NoteView>, ApiError> {
    let folder = ready(&state, &me).await?;
    trashed_note(&state, me.user.id, folder, id).await?;
    let item = by(&state, &me).restore(id).await?;
    let indexed = state.store.add(me.user.id, &item).await?;
    let file = NoteFile::parse(&state.store.read(id).await?);
    Ok(Json(NoteView::new(indexed, file.body().to_owned(), folder)))
}

/// `DELETE /trash/{id}`: deletes a note in the trash forever.
async fn delete_forever(
    State(state): State<NotesState>,
    Authenticated(me): Authenticated,
    Path(id): Path<ItemId>,
) -> Result<StatusCode, ApiError> {
    let folder = ready(&state, &me).await?;
    trashed_note(&state, me.user.id, folder, id).await?;
    by(&state, &me).delete_forever(id).await?;
    Ok(StatusCode::NO_CONTENT)
}

/// `DELETE /trash`: deletes every note in the trash forever. Other files
/// in Drive's trash stay.
async fn empty_trash(
    State(state): State<NotesState>,
    Authenticated(me): Authenticated,
) -> Result<StatusCode, ApiError> {
    let folder = ready(&state, &me).await?;
    let storage = by(&state, &me);
    for item in trashed_notes(&state, me.user.id, folder).await? {
        storage.delete_forever(item.id).await?;
    }
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Serialize)]
struct LabelsView {
    labels: Vec<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct LabelName {
    name: String,
}

/// `GET /labels`: the person's labels, in order.
async fn labels(
    State(state): State<NotesState>,
    Authenticated(me): Authenticated,
) -> Result<Json<LabelsView>, ApiError> {
    ready(&state, &me).await?;
    Ok(Json(LabelsView {
        labels: state.store.labels(me.user.id).await?,
    }))
}

/// `POST /labels`: makes a label, before any note has it.
async fn add_label(
    State(state): State<NotesState>,
    Authenticated(me): Authenticated,
    Json(request): Json<LabelName>,
) -> Result<Json<LabelsView>, ApiError> {
    ready(&state, &me).await?;
    let label = check_label(&request.name)?;
    state.store.keep_label(me.user.id, label, true).await?;
    Ok(Json(LabelsView {
        labels: state.store.labels(me.user.id).await?,
    }))
}

/// Changes the label `from` on every note that has it: to `to`, or away.
async fn relabel(
    state: &NotesState,
    me: &SignedIn,
    from: &str,
    to: Option<&str>,
) -> Result<(), ApiError> {
    let folder = ready(state, me).await?;
    for indexed in state.store.all(me.user.id).await? {
        if !indexed
            .details
            .tags
            .iter()
            .any(|tag| tag.eq_ignore_ascii_case(from))
        {
            continue;
        }
        let mut tags: Vec<String> = Vec::new();
        for tag in indexed.details.tags {
            let tag = if tag.eq_ignore_ascii_case(from) {
                match to {
                    Some(to) => to.to_owned(),
                    None => continue,
                }
            } else {
                tag
            };
            if !tags.iter().any(|seen| seen.eq_ignore_ascii_case(&tag)) {
                tags.push(tag);
            }
        }
        let item = own_note(state, me.user.id, folder, indexed.id).await?;
        let item = change_note(
            state,
            me,
            item,
            NoteChange {
                base: None,
                title: None,
                body: None,
                color: None,
                pinned: None,
                archived: None,
                tags: Some(tags),
            },
        )
        .await?;
        state.store.add(me.user.id, &item).await?;
    }
    state
        .store
        .keep_label(me.user.id, from.to_owned(), false)
        .await?;
    Ok(())
}

/// `PUT /labels/{name}`: renames a label on every note that has it.
async fn rename_label(
    State(state): State<NotesState>,
    Authenticated(me): Authenticated,
    Path(name): Path<String>,
    Json(request): Json<LabelName>,
) -> Result<Json<LabelsView>, ApiError> {
    let to = check_label(&request.name)?;
    relabel(&state, &me, &name, Some(&to)).await?;
    // Kept even if no note has it, as Keep does.
    state.store.keep_label(me.user.id, to, true).await?;
    Ok(Json(LabelsView {
        labels: state.store.labels(me.user.id).await?,
    }))
}

/// `DELETE /labels/{name}`: takes a label off every note, and forgets it.
async fn remove_label(
    State(state): State<NotesState>,
    Authenticated(me): Authenticated,
    Path(name): Path<String>,
) -> Result<Json<LabelsView>, ApiError> {
    relabel(&state, &me, &name, None).await?;
    Ok(Json(LabelsView {
        labels: state.store.labels(me.user.id).await?,
    }))
}
