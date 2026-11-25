//! Notes' JSON API, at `/api/notes`, for the web app and later the phone
//! app. It acts on the signed-in person's own notes, and the notes others
//! shared with them, which people edit together over `/notes/{id}/live`
//! (decision 31's second step).

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use accordion_core::identity::SignedIn;
use accordion_core::live::Hint;
use accordion_core::storage::{Access, Error, Item, ItemKind, Storage};
use accordion_module::{ApiError, Authenticated, coedit};
use accordion_types::{ItemId, UserId};
use axum::extract::{DefaultBodyLimit, Path, Query, State, WebSocketUpgrade};
use axum::http::StatusCode;
use axum::response::Response;
use axum::routing::{get, post, put};
use axum::{Json, Router};
use jiff::Timestamp;
use serde::{Deserialize, Serialize};

use crate::NotesState;
use crate::format::NoteFormat;
use crate::images::{self, ImageView, Images};
use crate::note::{self, Details, NoteFile};
use crate::reminders::{Invalid, REMINDERS, ReminderView, When};
use crate::settings::NotesSettings;
use crate::shared::Choices;
use crate::store::{Indexed, shorten};
use crate::transcripts;

/// How much of each note the list sends, for its card.
const CARD_CHARS: usize = 1500;

pub(crate) fn routes(state: NotesState) -> Router {
    Router::new()
        .route("/notes", get(list).post(create))
        .route("/notes/{id}", get(open).put(update).delete(trash))
        .route("/notes/{id}/live", get(live))
        .route(
            "/notes/{id}/images",
            post(images::add_image).layer(DefaultBodyLimit::max(images::MAX_IMAGE)),
        )
        .route(
            "/notes/{id}/memos",
            post(images::add_memo).layer(DefaultBodyLimit::max(images::MAX_IMAGE)),
        )
        .route(
            "/notes/{id}/image",
            get(images::image).delete(images::remove_image),
        )
        .route(
            "/notes/{id}/reminder",
            put(set_reminder).delete(remove_reminder),
        )
        .route("/notes/{id}/reminder/snooze", post(snooze_reminder))
        .route("/notes/{id}/reminder/done", post(finish_reminder))
        .route("/trash", get(trashed).delete(empty_trash))
        .route("/trash/{id}", axum::routing::delete(delete_forever))
        .route("/trash/{id}/restore", post(restore))
        .route("/labels", get(labels).post(add_label))
        .route("/labels/{name}", put(rename_label).delete(remove_label))
        .route("/settings", get(get_settings).put(put_settings))
        .route(
            "/transcripts",
            get(transcripts::get_setting).put(transcripts::put_setting),
        )
        .route("/transcripts/on", get(transcripts::get_on))
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
    /// Whether anyone else can open it. Shared notes are edited together
    /// over `/notes/{id}/live`.
    shared: bool,
    /// Who owns it, when it's someone else's.
    owner: Option<PersonView>,
    /// `owner`, `editor`, or `viewer`.
    access: &'static str,
    /// The reader's own reminder on it, if they set one. Nobody else's
    /// shows.
    reminder: Option<ReminderView>,
    /// The images one of the reader's own notes shows, which move with it
    /// in Notes. Empty for others' notes, whose images show through
    /// `/notes/{id}/image` like anyone's.
    images: Vec<ImageView>,
    /// The names of its voice memos waiting for transcripts, which the
    /// editor shows as Transcribing….
    transcribing: Vec<String>,
}

#[derive(Clone, Serialize)]
struct PersonView {
    id: UserId,
    /// `None` for an account that was deleted.
    name: Option<String>,
}

fn access_name(access: Access) -> &'static str {
    match access {
        Access::Owner => "owner",
        Access::Editor => "editor",
        // Notes have no comments, so a commenter only views them.
        Access::Commenter | Access::Viewer => "viewer",
    }
}

impl NoteView {
    /// Someone else's note: its color from the file, and the rest from what
    /// the reader chose.
    fn others(
        item: &Item,
        access: Access,
        text: &str,
        body: String,
        choices: Choices,
        owner: PersonView,
    ) -> Self {
        let file = NoteFile::parse(text);
        let hash = match &item.kind {
            ItemKind::File { blob, .. } => blob.hash.to_hex(),
            ItemKind::Folder => String::new(),
        };
        Self {
            id: item.id,
            title: note::title_of(&item.name).to_owned(),
            titled: note::has_title(&item.name, file.body()),
            body,
            color: file.details().color,
            pinned: choices.pinned,
            archived: choices.archived,
            tags: choices.tags,
            folder: None,
            hash,
            created_at: item.created_at.clone(),
            modified_at: item.modified_at.clone(),
            shared: true,
            owner: Some(owner),
            access: access_name(access),
            reminder: None,
            images: Vec::new(),
            transcribing: Vec::new(),
        }
    }

    fn new(indexed: Indexed, body: String, top: ItemId) -> Self {
        Self {
            shared: false,
            owner: None,
            access: "owner",
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
            reminder: None,
            images: Vec::new(),
            transcribing: Vec::new(),
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
pub(crate) fn by(state: &NotesState, me: &SignedIn) -> Storage {
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

/// Which of someone's own notes anyone else can open: those shared
/// directly, and those in a shared folder or a shared Notes place.
struct Sharing {
    shared: HashSet<ItemId>,
    parents: HashMap<ItemId, Option<ItemId>>,
}

impl Sharing {
    async fn of(state: &NotesState, user: UserId, top: ItemId) -> Result<Self, ApiError> {
        Ok(Self {
            shared: state
                .storage
                .shared_by_owner(user)
                .await?
                .into_iter()
                .collect(),
            parents: state
                .store
                .folders(top)
                .await?
                .into_iter()
                .map(|folder| (folder.id, folder.parent))
                .collect(),
        })
    }

    /// Whether the note `id`, in `folder`, is shared.
    fn covers(&self, id: ItemId, folder: Option<ItemId>) -> bool {
        if self.shared.contains(&id) {
            return true;
        }
        // Each folder up to the top of the Notes place, whose own parent
        // isn't among the folders.
        let mut at = folder;
        while let Some(current) = at {
            if self.shared.contains(&current) {
                return true;
            }
            at = self.parents.get(&current).copied().flatten();
        }
        false
    }
}

/// People's names, looked up once each.
struct People<'a> {
    state: &'a NotesState,
    known: HashMap<UserId, PersonView>,
}

impl<'a> People<'a> {
    fn new(state: &'a NotesState) -> Self {
        Self {
            state,
            known: HashMap::new(),
        }
    }

    async fn get(&mut self, id: UserId) -> Result<PersonView, ApiError> {
        if let Some(person) = self.known.get(&id) {
            return Ok(person.clone());
        }
        let name = self
            .state
            .identity
            .user(id)
            .await?
            .map(|user| user.display_name);
        let person = PersonView { id, name };
        self.known.insert(id, person.clone());
        Ok(person)
    }
}

/// A note the signed-in person can reach: their own, or one shared with
/// them.
pub(crate) enum Reached {
    Own { item: Item, top: ItemId },
    Others { item: Item, access: Access },
}

pub(crate) async fn reach(
    state: &NotesState,
    me: &SignedIn,
    id: ItemId,
) -> Result<Reached, ApiError> {
    let top = ready(state, me).await?;
    let item = state
        .storage
        .item(id)
        .await?
        .ok_or_else(ApiError::not_found)?;
    if state.store.is_note(me.user.id, top, &item).await? {
        return Ok(Reached::Own { item, top });
    }
    match state.shared.note(me.user.id, id).await? {
        Some((item, access)) => Ok(Reached::Others { item, access }),
        None => Err(ApiError::not_found()),
    }
}

/// One of the person's own notes, as the web app shows it.
async fn own_view(
    state: &NotesState,
    me: &SignedIn,
    item: &Item,
    top: ItemId,
) -> Result<NoteView, ApiError> {
    let indexed = state.store.add(me.user.id, item).await?;
    let file = NoteFile::parse(&state.store.read(item.id).await?);
    let sharing = Sharing::of(state, me.user.id, top).await?;
    let shared = sharing.covers(indexed.id, indexed.folder);
    let reminder = state.reminders.get(me.user.id, item.id).await?;
    let images = Images::of(state, me.user.id, top)
        .await?
        .views(indexed.id, indexed.folder);
    Ok(NoteView {
        shared,
        images,
        reminder: reminder.as_ref().map(ReminderView::from),
        transcribing: transcribing(state, item.id).await?,
        ..NoteView::new(indexed, file.body().to_owned(), top)
    })
}

/// The names of a note's memos waiting for transcripts.
async fn transcribing(state: &NotesState, id: ItemId) -> Result<Vec<String>, ApiError> {
    Ok(state
        .waiting
        .by_note()
        .await?
        .remove(&id)
        .unwrap_or_default())
}

/// Someone else's note, as the web app shows it.
async fn others_view(
    state: &NotesState,
    me: &SignedIn,
    item: &Item,
    access: Access,
) -> Result<NoteView, ApiError> {
    let text = state.store.read(item.id).await?;
    let body = NoteFile::parse(&text).body().to_owned();
    let choices = state
        .shared
        .choices(me.user.id)
        .await?
        .remove(&item.id)
        .unwrap_or_default();
    let owner = People::new(state).get(item.owner).await?;
    let reminder = state.reminders.get(me.user.id, item.id).await?;
    Ok(NoteView {
        reminder: reminder.as_ref().map(ReminderView::from),
        transcribing: transcribing(state, item.id).await?,
        ..NoteView::others(item, access, &text, body, choices, owner)
    })
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

/// Whether a note's title, labels, or text has every word.
fn matches(words: &[String], name: &str, tags: &[String], body: &str) -> bool {
    if words.is_empty() {
        return true;
    }
    let text = format!("{}\n{}\n{}", note::title_of(name), tags.join("\n"), body).to_lowercase();
    words.iter().all(|word| text.contains(word))
}

/// `GET /notes`: every note, newest first, with the labels and folders.
/// Notes others shared show everywhere but in a folder.
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
    let sharing = Sharing::of(&state, me.user.id, top).await?;
    let images = Images::of(&state, me.user.id, top).await?;
    let mut notes: Vec<NoteView> = state
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
        .filter(|indexed| matches(&words, &indexed.name, &indexed.details.tags, &indexed.body))
        .map(|indexed| {
            let body = shorten(&indexed.body, CARD_CHARS);
            // Whether it has a title depends on the whole first line, which
            // the index has.
            let titled = note::has_title(&indexed.name, &indexed.body);
            let shared = sharing.covers(indexed.id, indexed.folder);
            NoteView {
                titled,
                shared,
                images: images.views(indexed.id, indexed.folder),
                ..NoteView::new(indexed, body, top)
            }
        })
        .collect();
    if within.is_none() {
        let mut choices = state.shared.choices(me.user.id).await?;
        let mut people = People::new(&state);
        let mut followed = HashSet::new();
        for (item, access) in state.shared.notes(me.user.id).await? {
            // Links in others' notes follow images that moved since.
            if followed.insert(item.owner) {
                images::owner_place(&state, item.owner).await?;
            }
            let text = state.store.read(item.id).await?;
            let mine = choices.remove(&item.id).unwrap_or_default();
            let body = NoteFile::parse(&text).body().to_owned();
            if !matches(&words, &item.name, &mine.tags, &body) {
                continue;
            }
            let owner = people.get(item.owner).await?;
            let short = shorten(&body, CARD_CHARS);
            notes.push(NoteView::others(&item, access, &text, short, mine, owner));
        }
        notes.sort_by(|a, b| b.modified_at.cmp(&a.modified_at));
    }
    let reminders = state.reminders.of(me.user.id).await?;
    let mut waiting = state.waiting.by_note().await?;
    for note in &mut notes {
        note.reminder = reminders.get(&note.id).map(ReminderView::from);
        note.transcribing = waiting.remove(&note.id).unwrap_or_default();
    }
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
        labels: all_labels(&state, me.user.id).await?,
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
    match reach(&state, &me, id).await? {
        Reached::Own { item, top } => Ok(Json(own_view(&state, &me, &item, top).await?)),
        Reached::Others { item, access } => {
            Ok(Json(others_view(&state, &me, &item, access).await?))
        }
    }
}

/// `GET /notes/{id}/live`: a WebSocket for editing a note together with
/// everyone else who has it open (decision 30). Viewers see the edits but
/// can't make them.
async fn live(
    State(state): State<NotesState>,
    Authenticated(me): Authenticated,
    Path(id): Path<ItemId>,
    socket: WebSocketUpgrade,
) -> Result<Response, ApiError> {
    reach(&state, &me, id).await?;
    let connection = state
        .coediting
        .join(id, me.user.id, Some(me.session.id), Arc::new(NoteFormat))
        .await?;
    Ok(coedit::upgrade(socket, connection))
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
    Json(mut change): Json<NoteChange>,
) -> Result<Json<NoteView>, ApiError> {
    // Typing that people are doing together goes into the file first, so
    // this change lands on top of it, and they see the change as edits.
    state.coediting.save_now(id).await;
    match reach(&state, &me, id).await? {
        Reached::Own { item, top } => {
            let item = change_note(&state, &me, item, change).await?;
            Ok(Json(own_view(&state, &me, &item, top).await?))
        }
        Reached::Others { item, access } => {
            // Pins, the archive, and labels are the reader's own.
            if change.pinned.is_some() || change.archived.is_some() || change.tags.is_some() {
                let mut choices = state
                    .shared
                    .choices(me.user.id)
                    .await?
                    .remove(&item.id)
                    .unwrap_or_default();
                if let Some(pinned) = change.pinned.take() {
                    choices.pinned = pinned;
                }
                if let Some(archived) = change.archived.take() {
                    choices.archived = archived;
                }
                if let Some(tags) = change.tags.take() {
                    choices.tags = check_tags(tags)?;
                }
                state.shared.set(me.user.id, item.id, choices).await?;
            }
            let item = if change.title.is_some() || change.body.is_some() || change.color.is_some()
            {
                if access < Access::Editor {
                    return Err(ApiError::new(
                        StatusCode::FORBIDDEN,
                        "You can see this note, but not change it. Ask its owner to make you an editor.",
                    ));
                }
                change_note(&state, &me, item, change).await?
            } else {
                item
            };
            Ok(Json(others_view(&state, &me, &item, access).await?))
        }
    }
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
    state.coediting.save_now(id).await;
    images::trash_with(&state, &me, folder, id).await?;
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
    images::restore_with(&state, &me, id).await?;
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
    images::delete_with(&state, &me, id).await?;
    state.reminders.forget(id).await?;
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
        images::delete_with(&state, &me, item.id).await?;
        state.reminders.forget(item.id).await?;
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

/// `GET /settings`: the signed-in person's Notes settings.
async fn get_settings(
    State(state): State<NotesState>,
    Authenticated(me): Authenticated,
) -> Result<Json<NotesSettings>, ApiError> {
    Ok(Json(state.settings.get(me.user.id).await?))
}

/// `PUT /settings`: replaces them, so send every setting.
async fn put_settings(
    State(state): State<NotesState>,
    Authenticated(me): Authenticated,
    Json(settings): Json<NotesSettings>,
) -> Result<StatusCode, ApiError> {
    state.settings.set(me.user.id, settings).await?;
    Ok(StatusCode::NO_CONTENT)
}

/// `GET /labels`: the person's labels, in order.
async fn labels(
    State(state): State<NotesState>,
    Authenticated(me): Authenticated,
) -> Result<Json<LabelsView>, ApiError> {
    ready(&state, &me).await?;
    Ok(Json(LabelsView {
        labels: all_labels(&state, me.user.id).await?,
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
        labels: all_labels(&state, me.user.id).await?,
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
        let tags = renamed(indexed.details.tags, from, to);
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
    // And on notes others shared, where labels are the person's own.
    for (item, mut choices) in state.shared.choices(me.user.id).await? {
        if choices
            .tags
            .iter()
            .any(|tag| tag.eq_ignore_ascii_case(from))
        {
            choices.tags = renamed(choices.tags, from, to);
            state.shared.set(me.user.id, item, choices).await?;
        }
    }
    state
        .store
        .keep_label(me.user.id, from.to_owned(), false)
        .await?;
    Ok(())
}

/// `tags` with `from` renamed to `to`, or taken off without one, and no
/// label twice.
fn renamed(tags: Vec<String>, from: &str, to: Option<&str>) -> Vec<String> {
    let mut renamed: Vec<String> = Vec::new();
    for tag in tags {
        let tag = if tag.eq_ignore_ascii_case(from) {
            match to {
                Some(to) => to.to_owned(),
                None => continue,
            }
        } else {
            tag
        };
        if !renamed.iter().any(|seen| seen.eq_ignore_ascii_case(&tag)) {
            renamed.push(tag);
        }
    }
    renamed
}

/// Every label the person has: on their own notes, kept for later, and on
/// notes others shared with them.
async fn all_labels(state: &NotesState, user: UserId) -> Result<Vec<String>, ApiError> {
    let mut labels = state.store.labels(user).await?;
    for choices in state.shared.choices(user).await?.into_values() {
        for tag in choices.tags {
            if !labels.iter().any(|label| label.eq_ignore_ascii_case(&tag)) {
                labels.push(tag);
            }
        }
    }
    labels.sort_by_key(|label| label.to_lowercase());
    Ok(labels)
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
        labels: all_labels(&state, me.user.id).await?,
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
        labels: all_labels(&state, me.user.id).await?,
    }))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct NewReminder {
    /// The clock time, such as `2026-11-20T09:00`.
    local: String,
    /// The browser's time zone, such as `Europe/Amsterdam`.
    zone: String,
    /// `daily`, `weekly`, `monthly`, or `yearly`. Leave it out for once.
    repeat: Option<String>,
}

fn invalid_reminder(invalid: Invalid) -> ApiError {
    ApiError::new(
        StatusCode::BAD_REQUEST,
        match invalid {
            Invalid::Time => "Pick a date and time for the reminder.",
            Invalid::Zone => {
                "Accordion doesn't recognize this device's time zone. Check the time zone in \
                 its settings, then try again."
            }
            Invalid::Repeat => "A reminder can repeat daily, weekly, monthly, or yearly.",
        },
    )
}

/// Tells the person's other tabs that their reminders changed.
fn reminders_changed(state: &NotesState, user: UserId) {
    state.live.send(Hint::person(REMINDERS, user));
}

/// `PUT /notes/{id}/reminder`: sets the signed-in person's own reminder on
/// a note they can open, a viewer's too, in place of any they had.
async fn set_reminder(
    State(state): State<NotesState>,
    Authenticated(me): Authenticated,
    Path(id): Path<ItemId>,
    Json(request): Json<NewReminder>,
) -> Result<Json<ReminderView>, ApiError> {
    reach(&state, &me, id).await?;
    let when = When::parse(&request.local, &request.zone, request.repeat.as_deref())
        .map_err(invalid_reminder)?;
    let reminder = state
        .reminders
        .set(me.user.id, id, when, Timestamp::now())
        .await?
        .ok_or_else(|| invalid_reminder(Invalid::Time))?;
    reminders_changed(&state, me.user.id);
    Ok(Json(ReminderView::from(&reminder)))
}

/// `DELETE /notes/{id}/reminder`: takes the person's reminder off a note.
async fn remove_reminder(
    State(state): State<NotesState>,
    Authenticated(me): Authenticated,
    Path(id): Path<ItemId>,
) -> Result<StatusCode, ApiError> {
    reach(&state, &me, id).await?;
    if state.reminders.remove(me.user.id, id).await? {
        reminders_changed(&state, me.user.id);
    }
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Snooze {
    /// When to ring again, such as `2026-11-20T17:00:00Z`.
    until: String,
}

/// The furthest ahead a snooze goes.
const SNOOZE_AT_MOST: i64 = 366 * 86_400;

/// `POST /notes/{id}/reminder/snooze`: rings the person's reminder again
/// later. One that repeats carries on from its own times after that.
async fn snooze_reminder(
    State(state): State<NotesState>,
    Authenticated(me): Authenticated,
    Path(id): Path<ItemId>,
    Json(request): Json<Snooze>,
) -> Result<Json<ReminderView>, ApiError> {
    reach(&state, &me, id).await?;
    let now = Timestamp::now();
    let until: Timestamp = request
        .until
        .parse()
        .ok()
        .filter(|until: &Timestamp| {
            *until > now && until.as_second() - now.as_second() <= SNOOZE_AT_MOST
        })
        .ok_or_else(|| {
            ApiError::new(
                StatusCode::BAD_REQUEST,
                "Pick a time later today or within the next year to snooze until.",
            )
        })?;
    let reminder = state
        .reminders
        .snooze(me.user.id, id, until)
        .await?
        .ok_or_else(ApiError::not_found)?;
    reminders_changed(&state, me.user.id);
    Ok(Json(ReminderView::from(&reminder)))
}

/// `POST /notes/{id}/reminder/done`: finishes the person's reminder. One
/// that doesn't repeat goes, and one that does waits for its next time.
/// Replies with what's left, or null.
async fn finish_reminder(
    State(state): State<NotesState>,
    Authenticated(me): Authenticated,
    Path(id): Path<ItemId>,
) -> Result<Json<Option<ReminderView>>, ApiError> {
    reach(&state, &me, id).await?;
    let left = state.reminders.done(me.user.id, id).await?;
    reminders_changed(&state, me.user.id);
    Ok(Json(left.as_ref().map(ReminderView::from)))
}
