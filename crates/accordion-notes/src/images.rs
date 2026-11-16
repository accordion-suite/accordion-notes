//! Images and voice memos in notes (decisions 31 and 33): each one a file
//! beside its note, linked by its path from the top of the Notes place.
//!
//! Notes serves a note's images and memos to everyone who can open the
//! note, so they're never shared themselves. It serves only files the note links
//! to, from its owner's Notes place, so a link can't reach anything else.

use std::collections::{HashMap, HashSet};

use accordion_core::identity::SignedIn;
use accordion_core::storage::{Access, Error, Item, ItemKind};
use accordion_module::{ApiError, Authenticated};
use accordion_types::{ItemId, UserId};
use axum::Json;
use axum::body::Bytes;
use axum::extract::{Path, Query, State};
use axum::http::header::{
    ACCEPT_RANGES, CACHE_CONTROL, CONTENT_LENGTH, CONTENT_RANGE, CONTENT_SECURITY_POLICY,
    CONTENT_TYPE, ETAG, IF_NONE_MATCH, IF_RANGE, RANGE, X_CONTENT_TYPE_OPTIONS,
};
use axum::http::{HeaderMap, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncReadExt, AsyncSeekExt};

use crate::NotesState;
use crate::api::{Reached, by, reach};
use crate::links::{self, image_extension, media_extension, sound_extension};
use crate::note::{self, NoteFile};
use crate::store::{Beside, NoteLink, path_text};

/// The largest picture or voice memo Notes takes. A 30-minute memo at the
/// 64 kbit/s the app records at is about 15 MB.
pub(crate) const MAX_IMAGE: usize = 50 * 1024 * 1024;

/// The most of a memo sent for a request that asks for everything from a
/// point on, as players do, so playing a long memo never reads it whole
/// into memory. Players ask again for the rest.
const MOST_AT_ONCE: u64 = 2 * 1024 * 1024;

/// The files in someone's Notes place, found by path and by name.
pub(crate) struct Files {
    by_path: HashMap<String, Beside>,
    by_name: HashMap<String, Vec<Beside>>,
}

impl Files {
    pub(crate) async fn of(state: &NotesState, user: UserId) -> Result<Self, Error> {
        let mut by_path = HashMap::new();
        let mut by_name: HashMap<String, Vec<Beside>> = HashMap::new();
        for file in state.store.files(user).await? {
            let name = file.path.rsplit('/').next().unwrap_or_default().to_owned();
            by_name.entry(name).or_default().push(file.clone());
            by_path.insert(file.path.clone(), file);
        }
        Ok(Self { by_path, by_name })
    }

    /// The file a link points to, from a note in the folder at `dir`: its
    /// path from the top of the place, as Notes writes links, then from
    /// the note's folder, then, for a bare name, the one file with it.
    pub(crate) fn resolve(&self, dir: &str, target: &str) -> Option<&Beside> {
        let target = target.trim_start_matches("./");
        if let Some(file) = self.by_path.get(target) {
            return Some(file);
        }
        if !dir.is_empty()
            && let Some(file) = self.by_path.get(&format!("{dir}/{target}"))
        {
            return Some(file);
        }
        if !target.contains('/')
            && let Some([only]) = self.by_name.get(target).map(Vec::as_slice)
        {
            return Some(only);
        }
        None
    }
}

/// Each folder's path from the top of the Notes place, with the top's
/// being empty.
pub(crate) async fn folder_paths(
    state: &NotesState,
    top: ItemId,
) -> Result<HashMap<ItemId, String>, Error> {
    let folders = state.store.folders(top).await?;
    let parents: HashMap<ItemId, (Option<ItemId>, String)> = folders
        .into_iter()
        .map(|folder| (folder.id, (folder.parent, folder.name)))
        .collect();
    let mut paths = HashMap::from([(top, String::new())]);
    for id in parents.keys() {
        let mut names = Vec::new();
        let mut at = Some(*id);
        while let Some(current) = at {
            match parents.get(&current) {
                Some((parent, name)) => {
                    names.push(name.as_str());
                    at = *parent;
                }
                None => break,
            }
        }
        names.reverse();
        paths.insert(*id, names.join("/"));
    }
    Ok(paths)
}

/// The path of the folder `item` is in, from the top of the place.
async fn dir_of(state: &NotesState, top: ItemId, item: &Item) -> Result<String, Error> {
    let Some(parent) = item.parent else {
        return Ok(String::new());
    };
    Ok(state
        .storage
        .path_within(top, parent)
        .await?
        .map(|path| path_text(&path))
        .unwrap_or_default())
}

/// An image or memo of one of the reader's own notes, which moves with the
/// note when it moves in Notes.
#[derive(Clone, Serialize)]
pub(crate) struct ImageView {
    id: ItemId,
    name: String,
    /// Whether it's in the note's folder, so it moves with the note.
    beside: bool,
}

/// The images and memos a note shows, as files, in the order it shows them.
fn images_of<'a>(files: &'a Files, dir: &str, links: &[NoteLink]) -> Vec<&'a Beside> {
    let mut seen = HashSet::new();
    links
        .iter()
        .filter(|link| link.embed)
        .filter_map(|link| files.resolve(dir, &link.target))
        .filter(|file| media_extension(&file.path).is_some() && seen.insert(file.id))
        .collect()
}

/// What the reader's own notes need to know about their images: the
/// images each shows, and which of them other notes use too.
pub(crate) struct Images {
    files: Files,
    links: HashMap<ItemId, Vec<NoteLink>>,
    dirs: HashMap<ItemId, String>,
}

impl Images {
    pub(crate) async fn of(state: &NotesState, user: UserId, top: ItemId) -> Result<Self, Error> {
        Ok(Self {
            files: Files::of(state, user).await?,
            links: state.store.links(user).await?,
            dirs: folder_paths(state, top).await?,
        })
    }

    fn dir(&self, folder: Option<ItemId>) -> &str {
        folder
            .and_then(|folder| self.dirs.get(&folder))
            .map_or("", String::as_str)
    }

    /// The images the note `id`, in `folder`, shows.
    pub(crate) fn views(&self, id: ItemId, folder: Option<ItemId>) -> Vec<ImageView> {
        let Some(links) = self.links.get(&id) else {
            return Vec::new();
        };
        images_of(&self.files, self.dir(folder), links)
            .into_iter()
            .map(|file| ImageView {
                id: file.id,
                name: file.path.rsplit('/').next().unwrap_or_default().to_owned(),
                beside: folder == Some(file.parent),
            })
            .collect()
    }

    /// The images only the note `id` uses, among the owner's notes, which
    /// go to the trash with it. `folders` says where each note is.
    fn only_in(&self, id: ItemId, folders: &HashMap<ItemId, Option<ItemId>>) -> Vec<ItemId> {
        let own: Vec<ItemId> = match self.links.get(&id) {
            Some(links) => images_of(
                &self.files,
                self.dir(folders.get(&id).copied().flatten()),
                links,
            )
            .into_iter()
            .map(|file| file.id)
            .collect(),
            None => return Vec::new(),
        };
        let used_elsewhere = self.used_by_others(id, folders);
        own.into_iter()
            .filter(|file| !used_elsewhere.contains(file))
            .collect()
    }

    /// Every file a note other than `id` links to.
    fn used_by_others(
        &self,
        id: ItemId,
        folders: &HashMap<ItemId, Option<ItemId>>,
    ) -> HashSet<ItemId> {
        let mut used = HashSet::new();
        for (note, links) in &self.links {
            if *note == id {
                continue;
            }
            let dir = self.dir(folders.get(note).copied().flatten());
            for link in links {
                if let Some(file) = self.files.resolve(dir, &link.target) {
                    used.insert(file.id);
                }
            }
        }
        used
    }
}

/// Where each of `user`'s notes is.
async fn note_folders(
    state: &NotesState,
    user: UserId,
) -> Result<HashMap<ItemId, Option<ItemId>>, Error> {
    Ok(state
        .store
        .all(user)
        .await?
        .into_iter()
        .map(|indexed| (indexed.id, indexed.folder))
        .collect())
}

/// Moves the images only `note` uses to the trash, ahead of the note, and
/// remembers them so they come back with it.
pub(crate) async fn trash_with(
    state: &NotesState,
    me: &SignedIn,
    top: ItemId,
    note: ItemId,
) -> Result<(), ApiError> {
    let images = Images::of(state, me.user.id, top).await?;
    let folders = note_folders(state, me.user.id).await?;
    let mut trashed = Vec::new();
    let storage = by(state, me);
    for file in images.only_in(note, &folders) {
        match storage.delete(file).await {
            Ok(_) => trashed.push(file),
            Err(Error::InTrash | Error::NotFound) => {}
            Err(err) => return Err(err.into()),
        }
    }
    state.store.trashed_with(note, trashed).await?;
    Ok(())
}

/// Brings back the images that went to the trash with `note`.
pub(crate) async fn restore_with(
    state: &NotesState,
    me: &SignedIn,
    note: ItemId,
) -> Result<(), ApiError> {
    let storage = by(state, me);
    for file in state.store.take_trashed_with(note).await? {
        match storage.restore(file).await {
            // Someone took it out already, or another file has its name,
            // in which case it stays in the trash, as Drive leaves it.
            Ok(_) | Err(Error::NotInTrash | Error::NotFound | Error::NameTaken { .. }) => {}
            Err(err) => return Err(err.into()),
        }
    }
    Ok(())
}

/// Deletes for good the images that went to the trash with `note`.
pub(crate) async fn delete_with(
    state: &NotesState,
    me: &SignedIn,
    note: ItemId,
) -> Result<(), ApiError> {
    let storage = by(state, me);
    for file in state.store.take_trashed_with(note).await? {
        let in_trash = state
            .storage
            .item(file)
            .await?
            .is_some_and(|item| item.trashed_at.is_some());
        if in_trash {
            match storage.delete_forever(file).await {
                Ok(()) | Err(Error::NotFound) => {}
                Err(err) => return Err(err.into()),
            }
        }
    }
    Ok(())
}

/// The note's owner's Notes place, with their index brought up to date, so
/// links follow images that moved since.
pub(crate) async fn owner_place(state: &NotesState, owner: UserId) -> Result<ItemId, Error> {
    let top = state.store.folder(owner).await?;
    state.store.follow(owner, top).await?;
    Ok(top)
}

/// A note the reader can reach, with its owner's Notes place.
async fn note_and_place(
    state: &NotesState,
    me: &SignedIn,
    id: ItemId,
) -> Result<(Item, ItemId, Access), ApiError> {
    Ok(match reach(state, me, id).await? {
        Reached::Own { item, top } => (item, top, Access::Owner),
        Reached::Others { item, access } => {
            let top = owner_place(state, item.owner).await?;
            (item, top, access)
        }
    })
}

/// Whether the note's file links to `target`.
async fn links_to(state: &NotesState, id: ItemId, target: &str) -> Result<bool, Error> {
    let text = state.store.read(id).await?;
    Ok(links::links(NoteFile::parse(&text).body())
        .iter()
        .any(|link| link.target == target))
}

fn not_found() -> ApiError {
    ApiError::new(
        StatusCode::NOT_FOUND,
        "This picture or memo isn't in Notes anymore. It may have been moved out of the Notes folder or deleted.",
    )
}

#[derive(Deserialize)]
pub(crate) struct ImageQuery {
    /// The link's target, as the note has it.
    path: String,
    /// `small` for a card, or `large` for the editor.
    size: Option<String>,
}

/// `GET /notes/{id}/image?path=…`: one of the note's images or memos, for
/// anyone who can open the note. `size=small` sends Drive's thumbnail of a
/// picture when there is one. A memo comes in parts when the player asks.
pub(crate) async fn image(
    State(state): State<NotesState>,
    Authenticated(me): Authenticated,
    Path(id): Path<ItemId>,
    Query(query): Query<ImageQuery>,
    request: HeaderMap,
) -> Result<Response, ApiError> {
    let (item, top, access) = note_and_place(&state, &me, id).await?;
    // Others see only what the note shows. Typing people do together
    // reaches the file first, so an image someone just added counts.
    if access != Access::Owner && !links_to(&state, id, &query.path).await? {
        state.coediting.save_now(id).await;
        if !links_to(&state, id, &query.path).await? {
            return Err(not_found());
        }
    }
    let dir = dir_of(&state, top, &item).await?;
    let files = Files::of(&state, item.owner).await?;
    let file = files.resolve(&dir, &query.path).ok_or_else(not_found)?;
    let sound = sound_extension(&file.path);
    let Some(extension) = image_extension(&file.path).or(sound.clone()) else {
        return Err(not_found());
    };
    let file = state
        .storage
        .path_within(top, file.id)
        .await?
        .and_then(|mut path| path.pop())
        .ok_or_else(not_found)?;
    let ItemKind::File { blob, .. } = &file.kind else {
        return Err(not_found());
    };
    if let Some(extension) = sound {
        return send_sound(
            &state,
            &request,
            file.id,
            blob.size,
            blob.hash.to_hex(),
            &extension,
        )
        .await;
    }
    let small = query.size.as_deref() == Some("small");
    let tag = format!(
        "\"{}-{}\"",
        blob.hash.to_hex(),
        if small { "s" } else { "l" }
    );
    let mut headers = HeaderMap::new();
    headers.insert(X_CONTENT_TYPE_OPTIONS, HeaderValue::from_static("nosniff"));
    headers.insert(CONTENT_SECURITY_POLICY, HeaderValue::from_static("sandbox"));
    headers.insert(CACHE_CONTROL, HeaderValue::from_static("private, no-cache"));
    headers.insert(
        ETAG,
        HeaderValue::from_str(&tag).expect("hex is a valid header"),
    );
    if request
        .get(IF_NONE_MATCH)
        .is_some_and(|wanted| wanted.as_bytes() == tag.as_bytes())
    {
        return Ok((StatusCode::NOT_MODIFIED, headers).into_response());
    }
    let thumbnail = state
        .thumbnails
        .find(blob.hash)
        .filter(|found| found.kind == accordion_core::thumbnails::ThumbnailKind::Picture);
    let (content_type, path) = match (small, thumbnail, shown_as(&extension)) {
        (true, Some(thumbnail), _) => ("image/webp", Some(thumbnail.path)),
        (_, _, Some(content_type)) => (content_type, None),
        (_, thumbnail, None) => match state.thumbnails.picture(blob.hash) {
            Some(picture) => ("image/webp", Some(picture)),
            None => (
                "image/webp",
                Some(thumbnail.map(|thumbnail| thumbnail.path).ok_or_else(|| {
                    ApiError::new(
                        StatusCode::NOT_FOUND,
                        "Accordion is still making a picture of this photo. Try again in a minute.",
                    )
                })?),
            ),
        },
    };
    let bytes = match path {
        Some(path) => tokio::fs::read(path).await.map_err(|_| not_found())?,
        None => {
            let mut bytes = Vec::new();
            state
                .storage
                .open_file(file.id)
                .await?
                .read_to_end(&mut bytes)
                .await
                .map_err(Error::Io)?;
            bytes
        }
    };
    headers.insert(CONTENT_TYPE, HeaderValue::from_static(content_type));
    Ok((headers, bytes).into_response())
}

/// The media type browsers show a picture as, for the ones they show
/// themselves.
fn shown_as(extension: &str) -> Option<&'static str> {
    Some(match extension {
        "jpg" | "jpeg" => "image/jpeg",
        "png" => "image/png",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "avif" => "image/avif",
        _ => return None,
    })
}

/// The media type of a sound Notes plays.
fn played_as(extension: &str) -> &'static str {
    match extension {
        "webm" => "audio/webm",
        "m4a" => "audio/mp4",
        "ogg" | "opus" => "audio/ogg",
        "mp3" => "audio/mpeg",
        "wav" => "audio/wav",
        "flac" => "audio/flac",
        _ => "application/octet-stream",
    }
}

/// Sends a memo, or the part of it a `Range` header asks for. Players ask
/// for parts to skip ahead, and Safari plays nothing without them. This
/// follows Drive's downloads, which serve ranges the same way.
async fn send_sound(
    state: &NotesState,
    request: &HeaderMap,
    id: ItemId,
    size: u64,
    hash: String,
    extension: &str,
) -> Result<Response, ApiError> {
    let tag = format!("\"{hash}\"");
    let mut headers = HeaderMap::new();
    headers.insert(CONTENT_TYPE, HeaderValue::from_static(played_as(extension)));
    headers.insert(ACCEPT_RANGES, HeaderValue::from_static("bytes"));
    headers.insert(X_CONTENT_TYPE_OPTIONS, HeaderValue::from_static("nosniff"));
    headers.insert(CONTENT_SECURITY_POLICY, HeaderValue::from_static("sandbox"));
    headers.insert(CACHE_CONTROL, HeaderValue::from_static("private, no-cache"));
    headers.insert(
        ETAG,
        HeaderValue::from_str(&tag).expect("hex is a valid header"),
    );
    let asked = match requested_range(request, size, &tag) {
        Ok(asked) => asked,
        Err(Unsatisfiable) => {
            headers.insert(
                CONTENT_RANGE,
                HeaderValue::try_from(format!("bytes */{size}")).expect("digits are valid"),
            );
            return Ok((StatusCode::RANGE_NOT_SATISFIABLE, headers).into_response());
        }
    };
    if asked.is_none()
        && request
            .get(IF_NONE_MATCH)
            .is_some_and(|wanted| wanted.as_bytes() == tag.as_bytes())
    {
        return Ok((StatusCode::NOT_MODIFIED, headers).into_response());
    }
    let range = match &asked {
        Some(asked) => asked.start..asked.end.min(asked.start + MOST_AT_ONCE),
        None => 0..size,
    };
    let mut file = state.storage.open_file(id).await?;
    file.seek(std::io::SeekFrom::Start(range.start))
        .await
        .map_err(Error::Io)?;
    let mut bytes = Vec::new();
    file.take(range.end - range.start)
        .read_to_end(&mut bytes)
        .await
        .map_err(Error::Io)?;
    headers.insert(CONTENT_LENGTH, HeaderValue::from(bytes.len()));
    if asked.is_none() {
        return Ok((headers, bytes).into_response());
    }
    headers.insert(
        CONTENT_RANGE,
        HeaderValue::try_from(format!("bytes {}-{}/{size}", range.start, range.end - 1))
            .expect("digits are valid"),
    );
    Ok((StatusCode::PARTIAL_CONTENT, headers, bytes).into_response())
}

/// A `Range` header that asks only for bytes past the end of the file.
#[derive(Debug, PartialEq, Eq)]
struct Unsatisfiable;

/// The bytes a `Range` header asks for, out of a file of `size` bytes with
/// `etag`, as Drive reads them. `None` means the whole file: there's no
/// header, or one this doesn't serve, such as several ranges at once, or
/// an `If-Range` for an older version (RFC 9110, section 14).
fn requested_range(
    request: &HeaderMap,
    size: u64,
    etag: &str,
) -> Result<Option<std::ops::Range<u64>>, Unsatisfiable> {
    let Some(spec) = request
        .get(RANGE)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.trim().strip_prefix("bytes="))
    else {
        return Ok(None);
    };
    if request
        .get(IF_RANGE)
        .is_some_and(|value| value.as_bytes() != etag.as_bytes())
    {
        return Ok(None);
    }
    let Some((first, last)) = spec.trim().split_once('-') else {
        return Ok(None);
    };
    let number = |text: &str| text.trim().parse::<u64>().ok();
    let range = match (first.trim().is_empty(), last.trim().is_empty()) {
        // "-500": the last 500 bytes.
        (true, false) => match number(last) {
            Some(0) => return Err(Unsatisfiable),
            Some(count) => size.saturating_sub(count)..size,
            None => return Ok(None),
        },
        // "500-": from byte 500 to the end.
        (false, true) => match number(first) {
            Some(start) => start..size,
            None => return Ok(None),
        },
        // "500-999", which includes byte 999.
        (false, false) => match (number(first), number(last)) {
            (Some(start), Some(end)) if end >= start => start..size.min(end + 1),
            _ => return Ok(None),
        },
        (true, true) => return Ok(None),
    };
    if range.start >= size {
        return Err(Unsatisfiable);
    }
    Ok(Some(range))
}

#[derive(Deserialize)]
pub(crate) struct NewImage {
    /// The picture's own file name, for its extension.
    name: String,
}

#[derive(Serialize)]
pub(crate) struct AddedImage {
    /// Its path from the top of the Notes place, for the link.
    path: String,
    /// The line that shows it, for the top of the note.
    line: String,
}

/// Only an editor or the owner changes a note.
fn may_edit(access: Access) -> Result<(), ApiError> {
    if access == Access::Viewer {
        return Err(ApiError::new(
            StatusCode::FORBIDDEN,
            "You can see this note, but not change it. Ask its owner to make you an editor.",
        ));
    }
    Ok(())
}

/// `POST /notes/{id}/images?name=…`: saves a picture beside the note,
/// named after it, in its owner's Notes place, and replies with the line
/// that shows it. Adding the line to the note is up to the app, since a
/// shared note's text is everyone's to edit.
pub(crate) async fn add_image(
    State(state): State<NotesState>,
    Authenticated(me): Authenticated,
    Path(id): Path<ItemId>,
    Query(request): Query<NewImage>,
    body: Bytes,
) -> Result<(StatusCode, Json<AddedImage>), ApiError> {
    let Some(extension) = image_extension(&request.name) else {
        return Err(ApiError::new(
            StatusCode::BAD_REQUEST,
            "Notes takes JPEG, PNG, GIF, WebP, AVIF, and HEIC pictures. Choose one of those.",
        ));
    };
    if body.is_empty() {
        return Err(ApiError::new(
            StatusCode::BAD_REQUEST,
            "This picture is empty. Choose another one.",
        ));
    }
    save_beside(&state, &me, id, &extension, &body).await
}

/// What browsers record voice memos as.
const RECORDED_EXTENSIONS: [&str; 3] = ["webm", "m4a", "ogg"];

/// `POST /notes/{id}/memos?name=…`: saves a voice memo recorded in Notes
/// beside the note, as `add_image` saves a picture. Only memos recorded
/// in Notes come this way, which is how Notes will know which ones to
/// transcribe (decision 33).
pub(crate) async fn add_memo(
    State(state): State<NotesState>,
    Authenticated(me): Authenticated,
    Path(id): Path<ItemId>,
    Query(request): Query<NewImage>,
    body: Bytes,
) -> Result<(StatusCode, Json<AddedImage>), ApiError> {
    let extension = sound_extension(&request.name)
        .filter(|extension| RECORDED_EXTENSIONS.contains(&extension.as_str()))
        .ok_or_else(|| {
            ApiError::new(
                StatusCode::BAD_REQUEST,
                "Notes saves voice memos as WebM, MP4, or Ogg sound, which this isn't.",
            )
        })?;
    if body.is_empty() {
        return Err(ApiError::new(
            StatusCode::BAD_REQUEST,
            "This memo has no sound in it. Record it again.",
        ));
    }
    save_beside(&state, &me, id, &extension, &body).await
}

/// Saves a file beside the note `id`, named after it and numbered, and
/// replies with the line that shows or plays it.
async fn save_beside(
    state: &NotesState,
    me: &SignedIn,
    id: ItemId,
    extension: &str,
    body: &[u8],
) -> Result<(StatusCode, Json<AddedImage>), ApiError> {
    let (item, top, access) = note_and_place(state, me, id).await?;
    may_edit(access)?;
    let parent = item.parent.ok_or(Error::NotAFile)?;
    let title = note::title_of(&item.name);
    let storage = by(state, me);
    // Numbered past every file of the note's already there, whatever its
    // kind, so `Groceries 1.jpg` and `Groceries 1.webm` aren't both beside
    // it.
    let taken: HashSet<String> = state
        .storage
        .children(parent)
        .await?
        .into_iter()
        .filter_map(|child| {
            let (stem, _) = child.name.rsplit_once('.')?;
            Some(stem.to_lowercase())
        })
        .collect();
    let mut saved = None;
    for number in 1..=1000 {
        if taken.contains(&format!("{title} {number}").to_lowercase()) {
            continue;
        }
        let name = format!("{title} {number}.{extension}");
        match storage.create_file(parent, &name, body).await {
            Err(Error::NameTaken { .. }) => continue,
            result => {
                saved = Some(result?);
                break;
            }
        }
    }
    let saved = saved.ok_or(Error::NameTaken { folder: false })?;
    let path = state
        .storage
        .path_within(top, saved.id)
        .await?
        .map(|path| path_text(&path))
        .ok_or_else(not_found)?;
    // So the file is found by its path straight away.
    state.store.follow(item.owner, top).await?;
    Ok((
        StatusCode::CREATED,
        Json(AddedImage {
            line: links::image_line(&path),
            path,
        }),
    ))
}

/// `DELETE /notes/{id}/image?path=…`: after the app took an image or memo
/// out of the note, moves its file to the trash, unless the note or another of its
/// owner's notes still uses it.
pub(crate) async fn remove_image(
    State(state): State<NotesState>,
    Authenticated(me): Authenticated,
    Path(id): Path<ItemId>,
    Query(query): Query<ImageQuery>,
) -> Result<StatusCode, ApiError> {
    state.coediting.save_now(id).await;
    let (item, top, access) = note_and_place(&state, &me, id).await?;
    may_edit(access)?;
    if links_to(&state, id, &query.path).await? {
        return Ok(StatusCode::NO_CONTENT);
    }
    let dir = dir_of(&state, top, &item).await?;
    let images = Images::of(&state, item.owner, top).await?;
    let Some(file) = images.files.resolve(&dir, &query.path).cloned() else {
        return Ok(StatusCode::NO_CONTENT);
    };
    if media_extension(&file.path).is_none() {
        return Ok(StatusCode::NO_CONTENT);
    }
    let folders = note_folders(&state, item.owner).await?;
    if images.used_by_others(id, &folders).contains(&file.id) {
        return Ok(StatusCode::NO_CONTENT);
    }
    match by(&state, &me).delete(file.id).await {
        Ok(_) | Err(Error::InTrash | Error::NotFound) => Ok(StatusCode::NO_CONTENT),
        Err(err) => Err(err.into()),
    }
}
