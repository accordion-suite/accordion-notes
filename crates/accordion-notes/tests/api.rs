//! Notes' API, mounted the way the server mounts it, with two people.

use std::time::Duration;

use accordion_core::coedit::{CoEditing, Timing};
use accordion_core::db::Database;
use accordion_core::events::{Actor, Events, Via};
use accordion_core::identity::{AuthMethod, Client, Identity, NewUser};
use accordion_core::live::Live;
use accordion_core::storage::{ItemKind, ShareLevel, Storage};
use accordion_core::takeout::{self, Imports, Takeout};
use accordion_core::texts::{self, Texts};
use accordion_core::thumbnails::{Thumbnails, Tools};
use accordion_core::transcripts::Transcripts;
use accordion_module::{Module, Services, Stopping};
use accordion_notes::Notes;
use accordion_types::{ItemId, Role, UserId};
use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use serde_json::{Value, json};
use tokio::sync::watch;
use tower::ServiceExt;

/// Saves quickly, so tests see co-edited notes reach their files.
const QUICK: Timing = Timing {
    save_after: Duration::from_millis(200),
    save_at_least_every: Duration::from_secs(2),
    check_access_after: Duration::from_millis(50),
};

struct Server {
    _dir: tempfile::TempDir,
    _stop: watch::Sender<bool>,
    router: Router,
    storage: Storage,
    services: Services,
    alex: Person,
    sam: Person,
}

struct Person {
    id: UserId,
    bearer: String,
}

async fn start() -> Server {
    let dir = tempfile::tempdir().unwrap();
    let db = Database::open(&dir.path().join("accordion.db")).unwrap();
    let identity = Identity::new(db.clone()).await.unwrap();
    let storage = Storage::open(db.clone(), identity.clone(), &dir.path().join("storage"))
        .await
        .unwrap();
    // Notes never makes thumbnails or reads text with programs, so these
    // point nowhere.
    let thumbnails = Thumbnails::open(
        db.clone(),
        storage.clone(),
        dir.path(),
        Tools::installed("/nonexistent"),
    )
    .await
    .unwrap();
    let texts = Texts::open(
        db.clone(),
        storage.clone(),
        thumbnails.clone(),
        dir.path(),
        texts::Tools::installed("/nonexistent"),
    )
    .await
    .unwrap();
    let events = Events::open(db.clone()).await.unwrap();
    // Notes' Keep importer is tested in `keep.rs`, with a sandbox to read
    // archives.
    let takeout = Imports::open(
        db.clone(),
        storage.clone(),
        events.clone(),
        Takeout::new(takeout::Tools::installed("/nonexistent"), dir.path()),
    )
    .await
    .unwrap();
    let services = Services {
        db: db.clone(),
        coediting: CoEditing::open(db.clone(), storage.clone(), identity.clone(), QUICK)
            .await
            .unwrap(),
        identity: identity.clone(),
        storage: storage.clone(),
        events,
        thumbnails,
        texts,
        // Every memo says the same, instead of what whisper.cpp hears.
        transcripts: Transcripts::canned(db.clone(), storage.clone(), dir.path(), |_| {
            Some("Buy milk and bread.".to_owned())
        })
        .await
        .unwrap(),
        live: Live::new(db, &identity),
        link_origin: watch::channel(None).1,
        takeout,
    };
    let (stop, stopping) = watch::channel(false);
    let started = Notes
        .start(services.clone(), Stopping::new(stopping))
        .await
        .unwrap();
    // Reminders ringing and memos being transcribed.
    if let Some(background) = started.background {
        tokio::spawn(background);
    }
    Server {
        router: Router::new().nest("/api/notes", started.routes.api.unwrap()),
        // The first account is the server's owner, an admin.
        alex: person(&identity, "alex", Role::Owner).await,
        sam: person(&identity, "sam", Role::User).await,
        storage,
        services,
        _dir: dir,
        _stop: stop,
    }
}

async fn person(identity: &Identity, username: &str, role: Role) -> Person {
    let user = identity
        .create_user(NewUser {
            username: username.into(),
            display_name: username.into(),
            role,
            password: "correct horse".into(),
        })
        .await
        .unwrap();
    let (token, _) = identity
        .create_session(user.id, Client::App, "Laptop", AuthMethod::Password)
        .await
        .unwrap();
    Person {
        id: user.id,
        bearer: format!("Bearer {}", token.as_str()),
    }
}

impl Server {
    async fn api(
        &self,
        person: &Person,
        method: &str,
        path: &str,
        body: Option<Value>,
    ) -> (StatusCode, Value) {
        let mut request = Request::builder()
            .method(method)
            .uri(format!("/api/notes{path}"))
            .header("authorization", &person.bearer);
        let body = match body {
            Some(json) => {
                request = request.header("content-type", "application/json");
                Body::from(json.to_string())
            }
            None => Body::empty(),
        };
        let response = self
            .router
            .clone()
            .oneshot(request.body(body).unwrap())
            .await
            .unwrap();
        let status = response.status();
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        let value = if bytes.is_empty() {
            Value::Null
        } else {
            serde_json::from_slice(&bytes)
                .unwrap_or_else(|_| Value::String(String::from_utf8_lossy(&bytes).into_owned()))
        };
        (status, value)
    }

    /// A request with raw bytes, for pictures, and the raw reply.
    async fn raw(
        &self,
        person: &Person,
        method: &str,
        path: &str,
        body: Vec<u8>,
    ) -> (StatusCode, axum::http::HeaderMap, Vec<u8>) {
        self.raw_with(person, method, path, body, &[]).await
    }

    /// Like `raw`, with headers of its own, such as `Range`.
    async fn raw_with(
        &self,
        person: &Person,
        method: &str,
        path: &str,
        body: Vec<u8>,
        headers: &[(&str, &str)],
    ) -> (StatusCode, axum::http::HeaderMap, Vec<u8>) {
        let mut request = Request::builder()
            .method(method)
            .uri(format!("/api/notes{path}"))
            .header("authorization", &person.bearer);
        for (name, value) in headers {
            request = request.header(*name, *value);
        }
        let request = request.body(Body::from(body)).unwrap();
        let response = self.router.clone().oneshot(request).await.unwrap();
        let status = response.status();
        let headers = response.headers().clone();
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        (status, headers, bytes.to_vec())
    }

    /// Storage acting as `person`, the way Drive, WebDAV, or sync would.
    fn as_person(&self, person: &Person) -> Storage {
        self.storage
            .acting_as(Actor::person(person.id, Via::WebDav))
    }

    /// The person's Notes place, beside their My Drive.
    async fn notes_folder(&self, person: &Person) -> ItemId {
        self.storage.place_folder(person.id, "notes").await.unwrap()
    }

    async fn file_text(&self, id: &str) -> String {
        let id: ItemId = id.parse().unwrap();
        let mut file = self.storage.open_file(id).await.unwrap();
        let mut text = String::new();
        tokio::io::AsyncReadExt::read_to_string(&mut file, &mut text)
            .await
            .unwrap();
        text
    }

    async fn name_of(&self, id: &str) -> String {
        let id: ItemId = id.parse().unwrap();
        self.storage.item(id).await.unwrap().unwrap().name
    }

    async fn list(&self, person: &Person) -> Vec<Value> {
        let (status, body) = self.api(person, "GET", "/notes", None).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        body["notes"].as_array().unwrap().clone()
    }
}

fn hash_of(item: &accordion_core::storage::Item) -> accordion_core::storage::ContentHash {
    match &item.kind {
        ItemKind::File { blob, .. } => blob.hash,
        ItemKind::Folder => panic!("a folder"),
    }
}

#[tokio::test]
async fn takes_a_note_as_a_markdown_file_in_the_notes_folder() {
    let server = start().await;
    let (status, note) = server
        .api(
            &server.alex,
            "POST",
            "/notes",
            Some(json!({"body": "- [ ] Milk\n- [ ] Bread\n", "color": "yellow", "tags": ["Home"]})),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{note}");
    assert_eq!(note["title"], "Milk");
    assert_eq!(note["titled"], false);
    assert_eq!(note["color"], "yellow");

    let id = note["id"].as_str().unwrap();
    assert_eq!(server.name_of(id).await, "Milk.md");
    assert_eq!(
        server.file_text(id).await,
        "---\ncolor: yellow\ntags:\n  - Home\n---\n- [ ] Milk\n- [ ] Bread\n"
    );
    let folder = server.notes_folder(&server.alex).await;
    let children = server.storage.children(folder).await.unwrap();
    assert_eq!(children.len(), 1);

    let notes = server.list(&server.alex).await;
    assert_eq!(notes.len(), 1);
    assert_eq!(notes[0]["tags"], json!(["Home"]));
    assert!(server.list(&server.sam).await.is_empty());
}

#[tokio::test]
async fn a_note_without_a_title_follows_its_first_words() {
    let server = start().await;
    let (_, note) = server
        .api(
            &server.alex,
            "POST",
            "/notes",
            Some(json!({"body": "Call the dentist"})),
        )
        .await;
    let id = note["id"].as_str().unwrap();
    let (status, changed) = server
        .api(
            &server.alex,
            "PUT",
            &format!("/notes/{id}"),
            Some(json!({"base": note["hash"], "body": "Call the vet\nTomorrow"})),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{changed}");
    assert_eq!(server.name_of(id).await, "Call the vet.md");
    assert_eq!(changed["titled"], false);

    // A title someone gives stays when the text changes.
    let (_, titled) = server
        .api(
            &server.alex,
            "PUT",
            &format!("/notes/{id}"),
            Some(json!({"title": "Pets"})),
        )
        .await;
    assert_eq!(titled["title"], "Pets");
    assert_eq!(titled["titled"], true);
    server
        .api(
            &server.alex,
            "PUT",
            &format!("/notes/{id}"),
            Some(json!({"base": titled["hash"], "body": "Walk the dog"})),
        )
        .await;
    assert_eq!(server.name_of(id).await, "Pets.md");

    // Two notes with the same first words get numbers.
    let (_, again) = server
        .api(
            &server.alex,
            "POST",
            "/notes",
            Some(json!({"body": "Walk the dog", "title": "Pets"})),
        )
        .await;
    assert_eq!(
        server.name_of(again["id"].as_str().unwrap()).await,
        "Pets (2).md"
    );
    assert_eq!(again["title"], "Pets (2)");
}

#[tokio::test]
async fn refuses_to_save_over_a_change_it_didnt_see() {
    let server = start().await;
    let (_, note) = server
        .api(&server.alex, "POST", "/notes", Some(json!({"body": "One"})))
        .await;
    let id = note["id"].as_str().unwrap();
    let body = |text: &str| Some(json!({"base": note["hash"], "body": text}));
    let (status, _) = server
        .api(&server.alex, "PUT", &format!("/notes/{id}"), body("Two"))
        .await;
    assert_eq!(status, StatusCode::OK);
    let (status, _) = server
        .api(&server.alex, "PUT", &format!("/notes/{id}"), body("Three"))
        .await;
    assert_eq!(status, StatusCode::PRECONDITION_FAILED);
    // Changing only a pin doesn't need the hash, and keeps the text.
    let (status, pinned) = server
        .api(
            &server.alex,
            "PUT",
            &format!("/notes/{id}"),
            Some(json!({"pinned": true})),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(pinned["body"], "Two");
    assert_eq!(pinned["pinned"], true);
}

#[tokio::test]
async fn keeps_front_matter_from_obsidian() {
    let server = start().await;
    server.list(&server.alex).await;
    let folder = server.notes_folder(&server.alex).await;
    let text =
        "---\naliases: [Trip]\ntags:\n- travel\ncssclasses:\n  - wide\n---\nPack the tent.\n";
    let item = server
        .as_person(&server.alex)
        .create_file(folder, "Camping.md", text.as_bytes())
        .await
        .unwrap();

    let notes = server.list(&server.alex).await;
    assert_eq!(notes.len(), 1);
    assert_eq!(notes[0]["title"], "Camping");
    assert_eq!(notes[0]["titled"], true);
    assert_eq!(notes[0]["tags"], json!(["travel"]));

    let id = item.id.to_string();
    server
        .api(
            &server.alex,
            "PUT",
            &format!("/notes/{id}"),
            Some(json!({"color": "green", "archived": true})),
        )
        .await;
    assert_eq!(
        server.file_text(&id).await,
        "---\naliases: [Trip]\ntags:\n- travel\ncssclasses:\n  - wide\ncolor: green\narchived: true\n---\nPack the tent.\n"
    );
}

#[tokio::test]
async fn follows_changes_made_outside_notes() {
    let server = start().await;
    server.list(&server.alex).await;
    let folder = server.notes_folder(&server.alex).await;
    let storage = server.as_person(&server.alex);
    let inside = storage.create_folder(folder, "Work").await.unwrap();
    let deep = storage
        .create_file(inside.id, "Meeting.md", &b"Agenda"[..])
        .await
        .unwrap();
    storage
        .create_file(folder, "photo.jpg", &b"not a note"[..])
        .await
        .unwrap();
    let titles = |notes: Vec<Value>| -> Vec<String> {
        let mut titles: Vec<String> = notes
            .iter()
            .map(|n| n["title"].as_str().unwrap().to_owned())
            .collect();
        titles.sort();
        titles
    };
    assert_eq!(titles(server.list(&server.alex).await), ["Meeting"]);

    // Edited somewhere else, such as Obsidian through sync.
    storage
        .write_version(
            deep.id,
            hash_of(&deep),
            &b"---\npinned: true\n---\nAgenda, revised"[..],
        )
        .await
        .unwrap();
    let notes = server.list(&server.alex).await;
    assert_eq!(notes[0]["pinned"], true);
    assert_eq!(notes[0]["body"], "Agenda, revised");

    // A folder of notes moved out takes its notes along.
    let top = server.storage.top_folder(server.alex.id).await.unwrap();
    storage.move_item(inside.id, top, "Work").await.unwrap();
    assert!(server.list(&server.alex).await.is_empty());
    storage.move_item(inside.id, folder, "Work").await.unwrap();
    assert_eq!(titles(server.list(&server.alex).await), ["Meeting"]);

    // The Notes place itself can't be deleted.
    assert!(matches!(
        storage.delete(folder).await,
        Err(accordion_core::storage::Error::TopFolder)
    ));
}

#[tokio::test]
async fn leaves_a_notes_folder_in_my_drive_alone() {
    let server = start().await;
    let top = server.storage.top_folder(server.alex.id).await.unwrap();
    let storage = server.as_person(&server.alex);
    let folder = storage.create_folder(top, "Notes").await.unwrap();
    let ideas = storage
        .create_file(folder.id, "Ideas.md", &b"Paint the shed"[..])
        .await
        .unwrap();
    assert!(server.list(&server.alex).await.is_empty());
    assert_ne!(server.notes_folder(&server.alex).await, folder.id);

    // Moving it into the Notes place makes it a note.
    let place = server.notes_folder(&server.alex).await;
    storage
        .move_item(ideas.id, place, "Ideas.md")
        .await
        .unwrap();
    let notes = server.list(&server.alex).await;
    assert_eq!(notes.len(), 1);
    assert_eq!(notes[0]["title"], "Ideas");
    assert_eq!(notes[0]["folder"], Value::Null);
}

/// The notes' titles, sorted.
fn sorted_titles(notes: Vec<Value>) -> Vec<String> {
    let mut titles: Vec<String> = notes
        .iter()
        .map(|note| note["title"].as_str().unwrap().to_owned())
        .collect();
    titles.sort();
    titles
}

#[tokio::test]
async fn shows_folders_and_the_notes_in_each() {
    let server = start().await;
    let place = server.notes_folder(&server.alex).await;
    let storage = server.as_person(&server.alex);
    let recipes = storage.create_folder(place, "Recipes").await.unwrap();
    let soups = storage.create_folder(recipes.id, "Soups").await.unwrap();
    let work = storage.create_folder(place, "Work").await.unwrap();
    storage
        .create_file(soups.id, "Leek.md", &b"Leeks"[..])
        .await
        .unwrap();
    storage
        .create_file(work.id, "Agenda.md", &b"Agenda"[..])
        .await
        .unwrap();
    storage
        .create_file(place, "Loose.md", &b"Loose"[..])
        .await
        .unwrap();

    let (status, all) = server.api(&server.alex, "GET", "/notes", None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(all["notes"].as_array().unwrap().len(), 3);
    assert_eq!(all["top"], place.to_string());
    let folders = all["folders"].as_array().unwrap();
    assert_eq!(folders.len(), 3);
    let soups_view = folders
        .iter()
        .find(|folder| folder["name"] == "Soups")
        .unwrap();
    assert_eq!(soups_view["parent"], recipes.id.to_string());
    let recipes_view = folders
        .iter()
        .find(|folder| folder["name"] == "Recipes")
        .unwrap();
    assert_eq!(recipes_view["parent"], Value::Null);

    // A folder shows the notes in it and in the folders inside it.
    let in_folder = |folder: ItemId| {
        let server = &server;
        async move {
            let (status, body) = server
                .api(
                    &server.alex,
                    "GET",
                    &format!("/notes?folder={folder}"),
                    None,
                )
                .await;
            assert_eq!(status, StatusCode::OK, "{body}");
            sorted_titles(body["notes"].as_array().unwrap().clone())
        }
    };
    assert_eq!(in_folder(recipes.id).await, ["Leek"]);
    assert_eq!(in_folder(work.id).await, ["Agenda"]);

    // A note taken in a folder is saved there.
    let (status, note) = server
        .api(
            &server.alex,
            "POST",
            "/notes",
            Some(json!({"body": "Minestrone", "folder": soups.id})),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{note}");
    assert_eq!(note["folder"], soups.id.to_string());
    let id: ItemId = note["id"].as_str().unwrap().parse().unwrap();
    let item = server.storage.item(id).await.unwrap().unwrap();
    assert_eq!(item.parent, Some(soups.id));
    assert_eq!(in_folder(recipes.id).await.len(), 2);

    // A note moved to another folder in Drive follows.
    storage.move_item(id, work.id, &item.name).await.unwrap();
    assert_eq!(in_folder(recipes.id).await, ["Leek"]);
    assert_eq!(in_folder(work.id).await.len(), 2);

    // Folders outside the Notes place aren't Notes' to show or fill.
    let top = server.storage.top_folder(server.alex.id).await.unwrap();
    let elsewhere = storage.create_folder(top, "Elsewhere").await.unwrap();
    let (status, _) = server
        .api(
            &server.alex,
            "GET",
            &format!("/notes?folder={}", elsewhere.id),
            None,
        )
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let (status, _) = server
        .api(
            &server.alex,
            "POST",
            "/notes",
            Some(json!({"body": "Stray", "folder": elsewhere.id})),
        )
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn other_people_cant_reach_a_note() {
    let server = start().await;
    let (_, note) = server
        .api(
            &server.alex,
            "POST",
            "/notes",
            Some(json!({"body": "Secret"})),
        )
        .await;
    let path = format!("/notes/{}", note["id"].as_str().unwrap());
    let (status, _) = server.api(&server.sam, "GET", &path, None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let (status, _) = server
        .api(&server.sam, "PUT", &path, Some(json!({"pinned": true})))
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let (status, _) = server.api(&server.sam, "DELETE", &path, None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let (status, _) = server.api(&server.alex, "GET", &path, None).await;
    assert_eq!(status, StatusCode::OK);
}

#[tokio::test]
async fn the_trash_holds_only_notes() {
    let server = start().await;
    let (_, note) = server
        .api(
            &server.alex,
            "POST",
            "/notes",
            Some(json!({"body": "Old idea"})),
        )
        .await;
    let id = note["id"].as_str().unwrap();
    let top = server.storage.top_folder(server.alex.id).await.unwrap();
    let storage = server.as_person(&server.alex);
    let other = storage
        .create_file(top, "report.md", &b"x"[..])
        .await
        .unwrap();
    storage.delete(other.id).await.unwrap();

    let (status, _) = server
        .api(&server.alex, "DELETE", &format!("/notes/{id}"), None)
        .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    assert!(server.list(&server.alex).await.is_empty());
    let (_, trash) = server.api(&server.alex, "GET", "/trash", None).await;
    assert_eq!(trash["notes"].as_array().unwrap().len(), 1);
    assert_eq!(trash["notes"][0]["body"], "Old idea");

    let (status, _) = server
        .api(&server.alex, "POST", &format!("/trash/{id}/restore"), None)
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(server.list(&server.alex).await.len(), 1);

    server
        .api(&server.alex, "DELETE", &format!("/notes/{id}"), None)
        .await;
    let (status, _) = server.api(&server.alex, "DELETE", "/trash", None).await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let (_, trash) = server.api(&server.alex, "GET", "/trash", None).await;
    assert!(trash["notes"].as_array().unwrap().is_empty());
    // Drive's own file is still in Drive's trash.
    assert_eq!(server.storage.trash(server.alex.id).await.unwrap().len(), 1);
}

#[tokio::test]
async fn labels_rename_and_go_on_every_note() {
    let server = start().await;
    for body in ["Milk", "Eggs"] {
        server
            .api(
                &server.alex,
                "POST",
                "/notes",
                Some(json!({"body": body, "tags": ["Shopping", "Home"]})),
            )
            .await;
    }
    let (_, labels) = server
        .api(
            &server.alex,
            "POST",
            "/labels",
            Some(json!({"name": "Ideas"})),
        )
        .await;
    assert_eq!(labels["labels"], json!(["Home", "Ideas", "Shopping"]));

    let (status, labels) = server
        .api(
            &server.alex,
            "PUT",
            "/labels/shopping",
            Some(json!({"name": "Groceries"})),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{labels}");
    assert_eq!(labels["labels"], json!(["Groceries", "Home", "Ideas"]));
    for note in server.list(&server.alex).await {
        assert_eq!(note["tags"], json!(["Groceries", "Home"]));
    }

    let (_, labels) = server
        .api(&server.alex, "DELETE", "/labels/Home", None)
        .await;
    assert_eq!(labels["labels"], json!(["Groceries", "Ideas"]));
    for note in server.list(&server.alex).await {
        assert_eq!(note["tags"], json!(["Groceries"]));
    }
}

#[tokio::test]
async fn searches_titles_labels_and_text() {
    let server = start().await;
    server
        .api(
            &server.alex,
            "POST",
            "/notes",
            Some(json!({"title": "Garden", "body": "Plant tomatoes in May"})),
        )
        .await;
    server
        .api(
            &server.alex,
            "POST",
            "/notes",
            Some(json!({"body": "Buy paint", "tags": ["House"]})),
        )
        .await;
    let search = |q: &str| {
        let path = format!("/notes?q={q}");
        let server = &server;
        async move {
            let (_, body) = server.api(&server.alex, "GET", &path, None).await;
            body["notes"]
                .as_array()
                .unwrap()
                .iter()
                .map(|n| n["title"].as_str().unwrap().to_owned())
                .collect::<Vec<_>>()
        }
    };
    assert_eq!(search("tomatoes").await, ["Garden"]);
    assert_eq!(search("house").await, ["Buy paint"]);
    assert_eq!(search("garden%20may").await, ["Garden"]);
    assert!(search("garden%20paint").await.is_empty());
}

#[tokio::test]
async fn reports_the_size_of_everyones_notes() {
    let server = start().await;
    server
        .api(
            &server.alex,
            "POST",
            "/notes",
            Some(json!({"body": "12345"})),
        )
        .await;
    assert_eq!(Notes.data_size(&server.services).await, Some(5));
}

#[tokio::test]
async fn keeps_each_persons_settings() {
    let server = start().await;
    let (status, body) = server.api(&server.alex, "GET", "/settings", None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        body,
        json!({"start_in": "text", "new_items": "bottom", "checked_at_bottom": true})
    );

    let (status, _) = server
        .api(
            &server.alex,
            "PUT",
            "/settings",
            Some(json!({"start_in": "title", "new_items": "top", "checked_at_bottom": false})),
        )
        .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let (_, body) = server.api(&server.alex, "GET", "/settings", None).await;
    assert_eq!(
        body,
        json!({"start_in": "title", "new_items": "top", "checked_at_bottom": false})
    );

    // A web app from before the setting leaves it as it was.
    let (status, _) = server
        .api(
            &server.alex,
            "PUT",
            "/settings",
            Some(json!({"new_items": "bottom", "checked_at_bottom": false})),
        )
        .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let (_, body) = server.api(&server.alex, "GET", "/settings", None).await;
    assert_eq!(
        body,
        json!({"start_in": "title", "new_items": "bottom", "checked_at_bottom": false})
    );

    // Sam's stay as they were.
    let (_, body) = server.api(&server.sam, "GET", "/settings", None).await;
    assert_eq!(
        body,
        json!({"start_in": "text", "new_items": "bottom", "checked_at_bottom": true})
    );

    for wrong in [
        json!({"new_items": "middle", "checked_at_bottom": true}),
        json!({"start_in": "label", "new_items": "top", "checked_at_bottom": true}),
    ] {
        let (status, _) = server
            .api(&server.alex, "PUT", "/settings", Some(wrong))
            .await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    }
}

/// Shares `id` from Alex with Sam.
async fn share_with_sam(server: &Server, id: &str, level: ShareLevel) {
    let id: ItemId = id.parse().unwrap();
    server
        .as_person(&server.alex)
        .share(id, server.sam.id, level, server.alex.id)
        .await
        .unwrap();
}

fn find<'a>(notes: &'a [Value], id: &str) -> Option<&'a Value> {
    notes.iter().find(|note| note["id"] == id)
}

#[tokio::test]
async fn shared_notes_show_for_both_with_each_persons_choices() {
    let server = start().await;
    let (_, note) = server
        .api(
            &server.alex,
            "POST",
            "/notes",
            Some(json!({"title": "Groceries", "body": "- [ ] Milk\n", "color": "yellow"})),
        )
        .await;
    let id = note["id"].as_str().unwrap().to_owned();
    assert_eq!(
        find(&server.list(&server.alex).await, &id).unwrap()["shared"],
        false
    );
    assert!(find(&server.list(&server.sam).await, &id).is_none());

    share_with_sam(&server, &id, ShareLevel::Viewer).await;
    assert_eq!(
        find(&server.list(&server.alex).await, &id).unwrap()["shared"],
        true
    );
    let sams = server.list(&server.sam).await;
    let shared = find(&sams, &id).unwrap();
    assert_eq!(shared["owner"]["name"], "alex");
    assert_eq!(shared["access"], "viewer");
    assert_eq!(shared["color"], "yellow");
    assert_eq!(shared["title"], "Groceries");

    // Sam's pin and labels are Sam's own, never in Alex's file.
    let before = server.file_text(&id).await;
    let (status, changed) = server
        .api(
            &server.sam,
            "PUT",
            &format!("/notes/{id}"),
            Some(json!({"pinned": true, "tags": ["Shop"]})),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{changed}");
    assert_eq!(changed["pinned"], true);
    assert_eq!(changed["tags"], json!(["Shop"]));
    assert_eq!(server.file_text(&id).await, before);
    let (_, labels) = server.api(&server.sam, "GET", "/labels", None).await;
    assert_eq!(labels["labels"], json!(["Shop"]));
    let (_, labels) = server.api(&server.alex, "GET", "/labels", None).await;
    assert_eq!(labels["labels"], json!([]));
    assert_eq!(
        find(&server.list(&server.alex).await, &id).unwrap()["pinned"],
        false
    );

    // A viewer can't change the note itself.
    let (status, _) = server
        .api(
            &server.sam,
            "PUT",
            &format!("/notes/{id}"),
            Some(json!({"color": "blue"})),
        )
        .await;
    assert_eq!(status, StatusCode::FORBIDDEN);

    // An editor can, and the color is the note's, so Alex sees it too.
    share_with_sam(&server, &id, ShareLevel::Editor).await;
    let (status, changed) = server
        .api(
            &server.sam,
            "PUT",
            &format!("/notes/{id}"),
            Some(json!({"color": "blue"})),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{changed}");
    assert!(server.file_text(&id).await.contains("color: blue"));
    assert_eq!(
        find(&server.list(&server.alex).await, &id).unwrap()["color"],
        "blue"
    );

    // Sam can't move it to the trash, which is Alex's.
    let (status, _) = server
        .api(&server.sam, "DELETE", &format!("/notes/{id}"), None)
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    // A Markdown file shared from My Drive stays in Drive.
    let top = server.storage.top_folder(server.alex.id).await.unwrap();
    let plan = server
        .as_person(&server.alex)
        .write_file(top, "Plan.md", &b"Not a note"[..])
        .await
        .unwrap();
    share_with_sam(&server, &plan.id.to_string(), ShareLevel::Editor).await;
    let sams = server.list(&server.sam).await;
    assert!(find(&sams, &plan.id.to_string()).is_none());
    let (status, _) = server
        .api(&server.sam, "GET", &format!("/notes/{}", plan.id), None)
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    // Once it's no longer shared, Sam can't reach it.
    let note_id: ItemId = id.parse().unwrap();
    server
        .storage
        .unshare(note_id, server.sam.id, server.alex.id)
        .await
        .unwrap();
    let (status, _) = server
        .api(&server.sam, "GET", &format!("/notes/{id}"), None)
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert!(find(&server.list(&server.sam).await, &id).is_none());
}

#[tokio::test]
async fn notes_in_a_shared_folder_are_shared() {
    let server = start().await;
    let notes = server.notes_folder(&server.alex).await;
    let trip = server
        .as_person(&server.alex)
        .create_folder(notes, "Trip")
        .await
        .unwrap();
    let (_, note) = server
        .api(
            &server.alex,
            "POST",
            "/notes",
            Some(json!({"body": "Passports", "folder": trip.id})),
        )
        .await;
    let id = note["id"].as_str().unwrap().to_owned();
    share_with_sam(&server, &trip.id.to_string(), ShareLevel::Editor).await;
    assert_eq!(
        find(&server.list(&server.alex).await, &id).unwrap()["shared"],
        true
    );
    let sams = server.list(&server.sam).await;
    assert_eq!(find(&sams, &id).unwrap()["access"], "editor");
}

mod live {
    use std::time::Duration;

    use futures_util::{SinkExt, StreamExt};
    use tokio_tungstenite::tungstenite::client::IntoClientRequest;
    use tokio_tungstenite::tungstenite::{self, Message};
    use yrs::encoding::read::Cursor;
    use yrs::sync::{Message as YMessage, MessageReader, SyncMessage};
    use yrs::updates::decoder::{Decode, DecoderV1};
    use yrs::updates::encoder::Encode;
    use yrs::{Doc, GetString, OffsetKind, Options, ReadTxn, StateVector, Text, Transact, Update};

    use super::*;

    type Socket = tokio_tungstenite::WebSocketStream<
        tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
    >;

    async fn serve(server: &Server) -> std::net::SocketAddr {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let router = server.router.clone();
        tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
        address
    }

    async fn connect(
        address: std::net::SocketAddr,
        person: &Person,
        id: &str,
    ) -> Result<Socket, tungstenite::Error> {
        let mut request = format!("ws://{address}/api/notes/notes/{id}/live")
            .into_client_request()
            .unwrap();
        request
            .headers_mut()
            .insert("authorization", person.bearer.parse().unwrap());
        tokio_tungstenite::connect_async(request)
            .await
            .map(|(socket, _)| socket)
    }

    /// A browser's copy of the note.
    struct Browser {
        doc: Doc,
        socket: Socket,
    }

    impl Browser {
        async fn open(address: std::net::SocketAddr, person: &Person, id: &str) -> Self {
            let socket = connect(address, person, id).await.unwrap();
            let doc = Doc::with_options(Options {
                offset_kind: OffsetKind::Bytes,
                ..Options::default()
            });
            let mut browser = Self { doc, socket };
            let step1 = YMessage::Sync(SyncMessage::SyncStep1(StateVector::default()));
            browser.send(step1).await;
            browser.take_in().await;
            browser
        }

        async fn send(&mut self, message: YMessage) {
            self.socket
                .send(Message::Binary(message.encode_v1().into()))
                .await
                .unwrap();
        }

        /// Applies what the server sends for a moment.
        async fn take_in(&mut self) {
            while let Ok(Some(Ok(message))) =
                tokio::time::timeout(Duration::from_millis(400), self.socket.next()).await
            {
                let Message::Binary(bytes) = message else {
                    continue;
                };
                let mut decoder = DecoderV1::new(Cursor::new(&bytes));
                for message in MessageReader::new(&mut decoder) {
                    if let YMessage::Sync(
                        SyncMessage::SyncStep2(update) | SyncMessage::Update(update),
                    ) = message.unwrap()
                    {
                        let update = Update::decode_v1(&update).unwrap();
                        self.doc.transact_mut().apply_update(update).unwrap();
                    }
                }
            }
        }

        /// Takes in what the server sends until `name` reads `expected`.
        async fn wait_for(&mut self, name: &str, expected: &str) {
            for _ in 0..25 {
                if self.text(name) == expected {
                    return;
                }
                self.take_in().await;
            }
            assert_eq!(self.text(name), expected);
        }

        fn text(&self, name: &str) -> String {
            let text = self.doc.get_or_insert_text(name);
            text.get_string(&self.doc.transact())
        }

        async fn type_at_end(&mut self, typed: &str) {
            let body = self.doc.get_or_insert_text("body");
            let before = self.doc.transact().state_vector();
            {
                let mut txn = self.doc.transact_mut();
                let end = body.len(&txn);
                body.insert(&mut txn, end, typed);
            }
            let update = self.doc.transact().encode_diff_v1(&before);
            self.send(YMessage::Sync(SyncMessage::Update(update))).await;
        }
    }

    #[tokio::test]
    async fn shared_notes_are_edited_together_and_reach_the_file() {
        let server = start().await;
        let address = serve(&server).await;
        let (_, note) = server
            .api(
                &server.alex,
                "POST",
                "/notes",
                Some(json!({"title": "Groceries", "body": "Milk", "color": "yellow"})),
            )
            .await;
        let id = note["id"].as_str().unwrap().to_owned();

        // Nobody reaches a note that isn't shared with them.
        assert!(connect(address, &server.sam, &id).await.is_err());

        share_with_sam(&server, &id, ShareLevel::Editor).await;
        let mut sam = Browser::open(address, &server.sam, &id).await;
        assert_eq!(sam.text("body"), "Milk");
        assert_eq!(sam.text("front"), "---\ncolor: yellow\n---\n");

        sam.type_at_end(" and eggs").await;
        tokio::time::sleep(Duration::from_millis(600)).await;
        assert_eq!(
            server.file_text(&id).await,
            "---\ncolor: yellow\n---\nMilk and eggs"
        );

        // A change through the API lands on top of the typing, and reaches
        // Sam's copy as edits. The server passes typing on only once it has
        // it, so Alex seeing it means the change can't overtake it.
        let mut alex = Browser::open(address, &server.alex, &id).await;
        sam.type_at_end(", bread").await;
        alex.wait_for("body", "Milk and eggs, bread").await;
        let (status, changed) = server
            .api(
                &server.alex,
                "PUT",
                &format!("/notes/{id}"),
                Some(json!({"color": "blue"})),
            )
            .await;
        assert_eq!(status, StatusCode::OK, "{changed}");
        assert_eq!(changed["body"], "Milk and eggs, bread");
        sam.take_in().await;
        assert_eq!(sam.text("front"), "---\ncolor: blue\n---\n");
        assert_eq!(sam.text("body"), "Milk and eggs, bread");
    }

    #[tokio::test]
    async fn viewers_watch_but_their_edits_dont_land() {
        let server = start().await;
        let address = serve(&server).await;
        let (_, note) = server
            .api(
                &server.alex,
                "POST",
                "/notes",
                Some(json!({"body": "Read me"})),
            )
            .await;
        let id = note["id"].as_str().unwrap().to_owned();
        share_with_sam(&server, &id, ShareLevel::Viewer).await;
        let mut sam = Browser::open(address, &server.sam, &id).await;
        sam.type_at_end(" not").await;
        tokio::time::sleep(Duration::from_millis(600)).await;
        assert_eq!(server.file_text(&id).await, "Read me");
    }
}

#[tokio::test]
async fn reminders_are_each_persons_own_and_stay_out_of_the_file() {
    let server = start().await;
    let (_, note) = server
        .api(
            &server.alex,
            "POST",
            "/notes",
            Some(json!({"title": "Groceries", "body": "Milk"})),
        )
        .await;
    let id = note["id"].as_str().unwrap().to_owned();
    let before = server.file_text(&id).await;
    let path = format!("/notes/{id}/reminder");

    // Nobody sets a reminder on a note they can't open.
    let monday =
        json!({"local": "2030-03-25T09:00", "zone": "Europe/Amsterdam", "repeat": "weekly"});
    let (status, _) = server
        .api(&server.sam, "PUT", &path, Some(monday.clone()))
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    let (status, set) = server.api(&server.alex, "PUT", &path, Some(monday)).await;
    assert_eq!(status, StatusCode::OK, "{set}");
    assert_eq!(set["local"], "2030-03-25T09:00");
    assert_eq!(set["repeat"], "weekly");
    // 09:00 in Amsterdam, which is in winter time that day.
    assert_eq!(set["due_at"], "2030-03-25T08:00:00Z");
    assert_eq!(set["rang"], false);
    assert_eq!(server.file_text(&id).await, before);

    // A viewer sets their own, and neither sees the other's.
    share_with_sam(&server, &id, ShareLevel::Viewer).await;
    assert_eq!(
        find(&server.list(&server.sam).await, &id).unwrap()["reminder"],
        Value::Null
    );
    let (status, sams) = server
        .api(
            &server.sam,
            "PUT",
            &path,
            Some(json!({"local": "2030-01-02T18:30", "zone": "UTC"})),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{sams}");
    assert_eq!(
        find(&server.list(&server.sam).await, &id).unwrap()["reminder"]["local"],
        "2030-01-02T18:30"
    );
    assert_eq!(
        find(&server.list(&server.alex).await, &id).unwrap()["reminder"]["local"],
        "2030-03-25T09:00"
    );
    let (_, opened) = server
        .api(&server.sam, "GET", &format!("/notes/{id}"), None)
        .await;
    assert_eq!(opened["reminder"]["zone"], "UTC");
    assert_eq!(server.file_text(&id).await, before);

    // Snoozing moves only the person's own, and done takes a reminder that
    // doesn't repeat away.
    let until = in_two_hours();
    let (status, snoozed) = server
        .api(
            &server.sam,
            "POST",
            &format!("{path}/snooze"),
            Some(json!({ "until": until })),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{snoozed}");
    assert_eq!(snoozed["due_at"], until);
    assert_eq!(snoozed["local"], "2030-01-02T18:30");
    let (status, left) = server
        .api(&server.sam, "POST", &format!("{path}/done"), None)
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(left, Value::Null);
    let (_, alexs_done) = server
        .api(&server.alex, "POST", &format!("{path}/done"), None)
        .await;
    assert_eq!(alexs_done["repeat"], "weekly");

    // Removed, it's gone for that person only.
    let (status, _) = server.api(&server.alex, "DELETE", &path, None).await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    assert_eq!(
        find(&server.list(&server.alex).await, &id).unwrap()["reminder"],
        Value::Null
    );
}

#[tokio::test]
async fn a_reminder_needs_a_real_time_zone_and_a_later_snooze() {
    let server = start().await;
    let (_, note) = server
        .api(&server.alex, "POST", "/notes", Some(json!({"body": "Vet"})))
        .await;
    let path = format!("/notes/{}/reminder", note["id"].as_str().unwrap());
    let (status, problem) = server
        .api(
            &server.alex,
            "PUT",
            &path,
            Some(json!({"local": "2030-01-02T18:30", "zone": "Nowhere/Here"})),
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(
        problem["error"].as_str().unwrap().contains("time zone"),
        "{problem}"
    );
    let (status, _) = server
        .api(
            &server.alex,
            "PUT",
            &path,
            Some(json!({"local": "tomorrow", "zone": "UTC"})),
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);

    // Snoozing needs a reminder, and a time to come.
    let snooze = format!("{path}/snooze");
    let (status, _) = server
        .api(
            &server.alex,
            "POST",
            &snooze,
            Some(json!({ "until": in_two_hours() })),
        )
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    server
        .api(
            &server.alex,
            "PUT",
            &path,
            Some(json!({"local": "2030-01-02T18:30", "zone": "UTC"})),
        )
        .await;
    let (status, _) = server
        .api(
            &server.alex,
            "POST",
            &snooze,
            Some(json!({"until": "2020-01-03T08:00:00Z"})),
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

/// Two hours from now, to the second, as the server writes times.
fn in_two_hours() -> String {
    let now = jiff::Timestamp::now().as_second();
    jiff::Timestamp::from_second(now + 7_200)
        .unwrap()
        .to_string()
}

/// Bytes that start like a JPEG, which is all Notes looks at.
const PHOTO: &[u8] = b"\xff\xd8\xff\xe0 a photo";

/// Adds a picture to a note and returns its path and line.
async fn add_photo(server: &Server, person: &Person, note: &str, name: &str) -> (String, String) {
    let (status, _, body) = server
        .raw(
            person,
            "POST",
            &format!("/notes/{note}/images?name={name}"),
            PHOTO.to_vec(),
        )
        .await;
    assert_eq!(
        status,
        StatusCode::CREATED,
        "{}",
        String::from_utf8_lossy(&body)
    );
    let added: Value = serde_json::from_slice(&body).unwrap();
    (
        added["path"].as_str().unwrap().to_owned(),
        added["line"].as_str().unwrap().to_owned(),
    )
}

/// Puts `line` at the top of a note's text.
async fn show_at_top(server: &Server, person: &Person, note: &str, line: &str) {
    let (_, opened) = server
        .api(person, "GET", &format!("/notes/{note}"), None)
        .await;
    let body = format!("{line}\n{}", opened["body"].as_str().unwrap());
    let (status, changed) = server
        .api(
            person,
            "PUT",
            &format!("/notes/{note}"),
            Some(json!({"base": opened["hash"], "body": body})),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{changed}");
}

fn image_address(note: &str, path: &str) -> String {
    format!(
        "/notes/{note}/image?path={}",
        path.replace('%', "%25")
            .replace(' ', "%20")
            .replace('/', "%2F")
    )
}

#[tokio::test]
async fn images_go_beside_their_note_and_links_follow_them() {
    let server = start().await;
    let top = server.notes_folder(&server.alex).await;
    let alex = server.as_person(&server.alex);
    let garden = alex.create_folder(top, "Garden").await.unwrap();
    let (_, note) = server
        .api(
            &server.alex,
            "POST",
            "/notes",
            Some(json!({"title": "Groceries", "body": "Milk", "folder": garden.id})),
        )
        .await;
    let id = note["id"].as_str().unwrap();

    let (path, line) = add_photo(&server, &server.alex, id, "IMG_0001.JPG").await;
    assert_eq!(path, "Garden/Groceries 1.jpg");
    assert_eq!(line, "![](Garden/Groceries%201.jpg)");
    let (second, _) = add_photo(&server, &server.alex, id, "b.png").await;
    assert_eq!(second, "Garden/Groceries 2.png");
    show_at_top(&server, &server.alex, id, &line).await;

    // The note's list shows only notes, and knows the image beside it.
    let notes = server.list(&server.alex).await;
    assert_eq!(notes.len(), 1);
    assert_eq!(notes[0]["images"][0]["name"], "Groceries 1.jpg");
    assert_eq!(notes[0]["images"][0]["beside"], true);

    let (status, headers, bytes) = server
        .raw(&server.alex, "GET", &image_address(id, &path), Vec::new())
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(headers["content-type"], "image/jpeg");
    assert_eq!(headers["content-security-policy"], "sandbox");
    assert_eq!(bytes, PHOTO);

    // Moved and renamed in Drive, the image keeps its link.
    let photo = server
        .storage
        .find(garden.id, vec!["Groceries 1.jpg".into()])
        .await
        .unwrap()
        .unwrap();
    alex.move_item(photo.id, top, "Moved.jpg").await.unwrap();
    server.list(&server.alex).await;
    assert!(server.file_text(id).await.contains("![](Moved.jpg)\n"));

    // So does an image whose folder moves.
    let pictures = alex.create_folder(top, "Pictures").await.unwrap();
    alex.move_item(photo.id, pictures.id, "Moved.jpg")
        .await
        .unwrap();
    server.list(&server.alex).await;
    alex.move_item(pictures.id, garden.id, "Pictures")
        .await
        .unwrap();
    server.list(&server.alex).await;
    let text = server.file_text(id).await;
    assert!(text.contains("![](Garden/Pictures/Moved.jpg)\n"), "{text}");
    let (status, _, _) = server
        .raw(
            &server.alex,
            "GET",
            &image_address(id, "Garden/Pictures/Moved.jpg"),
            Vec::new(),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
}

#[tokio::test]
async fn links_obsidian_writes_follow_their_files_too() {
    let server = start().await;
    server.list(&server.alex).await;
    let top = server.notes_folder(&server.alex).await;
    let alex = server.as_person(&server.alex);
    let garden = alex.create_folder(top, "Garden").await.unwrap();
    let pictures = alex.create_folder(top, "Pictures").await.unwrap();
    let mut photos = Vec::new();
    for name in ["a.jpg", "b.jpg", "d.jpg", "e.jpg"] {
        photos.push(alex.create_file(garden.id, name, PHOTO).await.unwrap().id);
    }
    let c = alex.create_file(pictures.id, "c.jpg", PHOTO).await.unwrap();
    // A bare name, links from the note's folder, one from the top of the
    // place, and a bare name to a file in another folder.
    let note = alex
        .create_file(
            garden.id,
            "Trip.md",
            &b"![[a.jpg]]\n![](b.jpg)\n![](./d.jpg)\n![](Garden/e.jpg)\n![[c.jpg]]\nTent\n"[..],
        )
        .await
        .unwrap();
    server.list(&server.alex).await;

    let sub = alex.create_folder(garden.id, "Sub").await.unwrap();
    let old = alex.create_folder(pictures.id, "Old").await.unwrap();
    alex.move_item(photos[0], garden.id, "a2.jpg")
        .await
        .unwrap();
    alex.move_item(photos[1], sub.id, "b.jpg").await.unwrap();
    alex.move_item(photos[2], top, "d.jpg").await.unwrap();
    alex.move_item(photos[3], garden.id, "e2.jpg")
        .await
        .unwrap();
    alex.move_item(c.id, old.id, "c.jpg").await.unwrap();
    server.list(&server.alex).await;

    // Each keeps its style where that still reaches the file. The bare
    // name to a file that's still the only one with it stays as it was.
    assert_eq!(
        server.file_text(&note.id.to_string()).await,
        "![[a2.jpg]]\n![](Sub/b.jpg)\n![](d.jpg)\n![](Garden/e2.jpg)\n![[c.jpg]]\nTent\n"
    );
    let notes = server.list(&server.alex).await;
    assert_eq!(notes[0]["images"].as_array().unwrap().len(), 5);
}

#[tokio::test]
async fn takes_only_pictures() {
    let server = start().await;
    let (_, note) = server
        .api(
            &server.alex,
            "POST",
            "/notes",
            Some(json!({"body": "Milk"})),
        )
        .await;
    let id = note["id"].as_str().unwrap();
    for name in ["page.svg", "notes.md", "noextension"] {
        let (status, _, _) = server
            .raw(
                &server.alex,
                "POST",
                &format!("/notes/{id}/images?name={name}"),
                PHOTO.to_vec(),
            )
            .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{name}");
    }
}

#[tokio::test]
async fn people_a_note_is_shared_with_see_its_images_and_nothing_else() {
    let server = start().await;
    let top = server.notes_folder(&server.alex).await;
    let (_, note) = server
        .api(
            &server.alex,
            "POST",
            "/notes",
            Some(json!({"title": "Trip", "body": "Bags"})),
        )
        .await;
    let id = note["id"].as_str().unwrap();
    let (path, line) = add_photo(&server, &server.alex, id, "a.jpg").await;
    show_at_top(&server, &server.alex, id, &line).await;
    server
        .as_person(&server.alex)
        .create_file(top, "Secret.jpg", PHOTO)
        .await
        .unwrap();

    // Nobody else sees it before the note is shared.
    let (status, _, _) = server
        .raw(&server.sam, "GET", &image_address(id, &path), Vec::new())
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    share_with_sam(&server, id, ShareLevel::Viewer).await;
    let (status, _, bytes) = server
        .raw(&server.sam, "GET", &image_address(id, &path), Vec::new())
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(bytes, PHOTO);
    for other in ["Secret.jpg", "Trip.md"] {
        let (status, _, _) = server
            .raw(&server.sam, "GET", &image_address(id, other), Vec::new())
            .await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{other}");
    }
    // The image isn't shared itself.
    let shared = server.storage.shared_with(server.sam.id).await.unwrap();
    assert_eq!(shared.len(), 1);

    // A viewer can't add one. An editor can, and its owner stores it.
    let (status, _, _) = server
        .raw(
            &server.sam,
            "POST",
            &format!("/notes/{id}/images?name=b.jpg"),
            PHOTO.to_vec(),
        )
        .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    share_with_sam(&server, id, ShareLevel::Editor).await;
    let (path, _) = add_photo(&server, &server.sam, id, "b.jpg").await;
    assert_eq!(path, "Trip 2.jpg");
    let added = server
        .storage
        .find(top, vec!["Trip 2.jpg".into()])
        .await
        .unwrap()
        .unwrap();
    assert_eq!(added.owner, server.alex.id);
}

#[tokio::test]
async fn images_go_to_the_trash_with_their_note_unless_another_uses_them() {
    let server = start().await;
    let top = server.notes_folder(&server.alex).await;
    let (_, first) = server
        .api(
            &server.alex,
            "POST",
            "/notes",
            Some(json!({"title": "First", "body": "One"})),
        )
        .await;
    let first = first["id"].as_str().unwrap();
    let (_, second) = server
        .api(
            &server.alex,
            "POST",
            "/notes",
            Some(json!({"title": "Second", "body": "Two"})),
        )
        .await;
    let second = second["id"].as_str().unwrap();
    let (_, only) = add_photo(&server, &server.alex, first, "a.jpg").await;
    let (_, both) = add_photo(&server, &server.alex, first, "b.jpg").await;
    show_at_top(&server, &server.alex, first, &format!("{only}\n{both}")).await;
    show_at_top(&server, &server.alex, second, &both).await;

    let named = |name: &'static str| {
        let storage = server.storage.clone();
        async move { storage.find(top, vec![name.into()]).await.unwrap() }
    };
    assert!(named("First 1.jpg").await.is_some());

    let (status, _) = server
        .api(&server.alex, "DELETE", &format!("/notes/{first}"), None)
        .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    assert!(named("First 1.jpg").await.is_none());
    assert!(named("First 2.jpg").await.is_some());

    let (status, _) = server
        .api(
            &server.alex,
            "POST",
            &format!("/trash/{first}/restore"),
            None,
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    let photo = named("First 1.jpg").await.unwrap();

    server
        .api(&server.alex, "DELETE", &format!("/notes/{first}"), None)
        .await;
    let (status, _) = server
        .api(&server.alex, "DELETE", &format!("/trash/{first}"), None)
        .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    assert!(server.storage.item(photo.id).await.unwrap().is_none());
    assert!(named("First 2.jpg").await.is_some());
}

#[tokio::test]
async fn a_removed_image_goes_to_the_trash_once_nothing_shows_it() {
    let server = start().await;
    let top = server.notes_folder(&server.alex).await;
    let (_, note) = server
        .api(
            &server.alex,
            "POST",
            "/notes",
            Some(json!({"title": "Shop", "body": "Eggs"})),
        )
        .await;
    let id = note["id"].as_str().unwrap();
    let (path, line) = add_photo(&server, &server.alex, id, "a.jpg").await;
    show_at_top(&server, &server.alex, id, &line).await;

    // Still shown, so it stays.
    let (status, _, _) = server
        .raw(
            &server.alex,
            "DELETE",
            &image_address(id, &path),
            Vec::new(),
        )
        .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    assert!(
        server
            .storage
            .find(top, vec![path.clone()])
            .await
            .unwrap()
            .is_some()
    );

    let (_, opened) = server
        .api(&server.alex, "GET", &format!("/notes/{id}"), None)
        .await;
    server
        .api(
            &server.alex,
            "PUT",
            &format!("/notes/{id}"),
            Some(json!({"base": opened["hash"], "body": "Eggs"})),
        )
        .await;
    server
        .raw(
            &server.alex,
            "DELETE",
            &image_address(id, &path),
            Vec::new(),
        )
        .await;
    assert!(
        server
            .storage
            .find(top, vec![path])
            .await
            .unwrap()
            .is_none()
    );
}

/// Saves a voice memo for a note, as the app does after recording, and
/// returns its path and line. `transcribe` is whether it was recorded for
/// a transcript.
async fn add_memo(
    server: &Server,
    person: &Person,
    note: &str,
    transcribe: bool,
) -> (String, String) {
    let (status, _, body) = server
        .raw(
            person,
            "POST",
            &format!("/notes/{note}/memos?name=Memo.webm&transcribe={transcribe}"),
            b"\x1a\x45\xdf\xa3 a memo".to_vec(),
        )
        .await;
    assert_eq!(
        status,
        StatusCode::CREATED,
        "{}",
        String::from_utf8_lossy(&body)
    );
    let added: Value = serde_json::from_slice(&body).unwrap();
    (
        added["path"].as_str().unwrap().to_owned(),
        added["line"].as_str().unwrap().to_owned(),
    )
}

/// Waits for a note's file to have `words` in it.
async fn wait_for_text(server: &Server, id: &str, words: &str) -> String {
    for _ in 0..100 {
        let text = server.file_text(id).await;
        if text.contains(words) {
            return text;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!(
        "{words:?} never reached the note: {}",
        server.file_text(id).await
    );
}

#[tokio::test]
async fn a_memos_transcript_lands_at_the_end_of_its_note() {
    let server = start().await;
    let (_, note) = server
        .api(
            &server.alex,
            "POST",
            "/notes",
            Some(json!({"title": "Shopping", "body": "Eggs"})),
        )
        .await;
    let id = note["id"].as_str().unwrap();
    let (path, line) = add_memo(&server, &server.alex, id, true).await;
    assert_eq!(path, "Shopping 1.webm");

    // It waits for the app to link the memo, and the editor says so.
    let (_, opened) = server
        .api(&server.alex, "GET", &format!("/notes/{id}"), None)
        .await;
    assert_eq!(opened["transcribing"], json!(["Shopping 1.webm"]));
    // What's typed meanwhile stays, with the words after it.
    let (status, changed) = server
        .api(
            &server.alex,
            "PUT",
            &format!("/notes/{id}"),
            Some(json!({"base": opened["hash"], "body": format!("{line}\nEggs\nTyped while recording")})),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{changed}");
    let text = wait_for_text(&server, id, "Buy milk and bread.").await;
    assert!(
        text.ends_with(
            "![](Shopping%201.webm)\nEggs\nTyped while recording\n\nBuy milk and bread.\n"
        ),
        "{text}"
    );
    let notes = server.list(&server.alex).await;
    assert_eq!(notes[0]["transcribing"], json!([]));
    assert!(
        notes[0]["body"]
            .as_str()
            .unwrap()
            .contains("Buy milk and bread.")
    );
}

#[tokio::test]
async fn a_memo_removed_before_its_transcript_gets_none() {
    let server = start().await;
    let (_, note) = server
        .api(
            &server.alex,
            "POST",
            "/notes",
            Some(json!({"title": "Shopping", "body": "Eggs"})),
        )
        .await;
    let id = note["id"].as_str().unwrap();
    let (path, _) = add_memo(&server, &server.alex, id, true).await;
    let memo = server
        .storage
        .find(server.notes_folder(&server.alex).await, vec![path.clone()])
        .await
        .unwrap()
        .unwrap();
    server
        .as_person(&server.alex)
        .delete(memo.id)
        .await
        .unwrap();
    for _ in 0..100 {
        let (_, opened) = server
            .api(&server.alex, "GET", &format!("/notes/{id}"), None)
            .await;
        if opened["transcribing"] == json!([]) {
            assert_eq!(opened["body"], "Eggs");
            return;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("the memo kept waiting");
}

#[tokio::test]
async fn memos_get_no_transcripts_while_its_off() {
    let server = start().await;
    server.services.transcripts.choose(None).await.unwrap();
    let (_, note) = server
        .api(
            &server.alex,
            "POST",
            "/notes",
            Some(json!({"title": "Shopping", "body": "Eggs"})),
        )
        .await;
    let id = note["id"].as_str().unwrap();
    add_memo(&server, &server.alex, id, true).await;
    let (_, opened) = server
        .api(&server.alex, "GET", &format!("/notes/{id}"), None)
        .await;
    assert_eq!(opened["transcribing"], json!([]));
}

#[tokio::test]
async fn a_memo_recorded_without_a_transcript_stays_plain() {
    let server = start().await;
    let (_, note) = server
        .api(
            &server.alex,
            "POST",
            "/notes",
            Some(json!({"title": "Shopping", "body": "Eggs"})),
        )
        .await;
    let id = note["id"].as_str().unwrap();
    let (_, line) = add_memo(&server, &server.alex, id, false).await;
    let (_, opened) = server
        .api(&server.alex, "GET", &format!("/notes/{id}"), None)
        .await;
    assert_eq!(opened["transcribing"], json!([]));
    let (status, changed) = server
        .api(
            &server.alex,
            "PUT",
            &format!("/notes/{id}"),
            Some(json!({"base": opened["hash"], "body": format!("{line}\nEggs")})),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{changed}");
    // Long enough for a transcript, which is canned here, to have landed.
    tokio::time::sleep(Duration::from_millis(500)).await;
    let text = server.file_text(id).await;
    assert!(!text.contains("Buy milk and bread."), "{text}");
}

#[tokio::test]
async fn everyone_can_see_whether_transcripts_are_on() {
    let server = start().await;
    let (status, on) = server
        .api(&server.sam, "GET", "/transcripts/on", None)
        .await;
    assert_eq!(status, StatusCode::OK, "{on}");
    assert_eq!(on, json!({"on": true}));
    server.services.transcripts.choose(None).await.unwrap();
    let (_, on) = server
        .api(&server.sam, "GET", "/transcripts/on", None)
        .await;
    assert_eq!(on, json!({"on": false}));
}

#[tokio::test]
async fn only_admins_see_and_change_the_transcript_setting() {
    let server = start().await;
    let (status, setting) = server.api(&server.alex, "GET", "/transcripts", None).await;
    assert_eq!(status, StatusCode::OK, "{setting}");
    assert_eq!(setting["recommended"], "small");
    assert_eq!(setting["models"][0]["id"], "base");
    let (status, _) = server.api(&server.sam, "GET", "/transcripts", None).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    let (status, _) = server
        .api(
            &server.sam,
            "PUT",
            "/transcripts",
            Some(json!({"model": null})),
        )
        .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    let (status, _) = server
        .api(
            &server.alex,
            "PUT",
            "/transcripts",
            Some(json!({"model": "large-v3-turbo"})),
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    let (status, setting) = server
        .api(
            &server.alex,
            "PUT",
            "/transcripts",
            Some(json!({"model": null})),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{setting}");
    assert_eq!(setting["model"], Value::Null);
    assert_eq!(setting["state"], "off");
}

#[tokio::test]
async fn voice_memos_go_beside_their_note_and_play_in_parts() {
    let server = start().await;
    let top = server.notes_folder(&server.alex).await;
    let (_, note) = server
        .api(
            &server.alex,
            "POST",
            "/notes",
            Some(json!({"title": "Trip", "body": "Bags"})),
        )
        .await;
    let id = note["id"].as_str().unwrap();
    let sound: Vec<u8> = (0..5000u32).map(|n| (n % 251) as u8).collect();

    // Only what browsers record comes this way.
    for name in ["memo.mp3", "memo.jpg", "memo"] {
        let (status, _, _) = server
            .raw(
                &server.alex,
                "POST",
                &format!("/notes/{id}/memos?name={name}"),
                sound.clone(),
            )
            .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{name}");
    }

    let (status, _, body) = server
        .raw(
            &server.alex,
            "POST",
            &format!("/notes/{id}/memos?name=memo.webm"),
            sound.clone(),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED);
    let added: Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(added["path"], "Trip 1.webm");
    assert_eq!(added["line"], "![](Trip%201.webm)");
    // Pictures number on past it.
    let (path, _) = add_photo(&server, &server.alex, id, "a.jpg").await;
    assert_eq!(path, "Trip 2.jpg");
    show_at_top(&server, &server.alex, id, "![](Trip%201.webm)").await;

    let address = image_address(id, "Trip 1.webm");
    let (status, headers, whole) = server.raw(&server.alex, "GET", &address, Vec::new()).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(headers["content-type"], "audio/webm");
    assert_eq!(headers["accept-ranges"], "bytes");
    assert_eq!(whole, sound);

    let (status, headers, part) = server
        .raw_with(
            &server.alex,
            "GET",
            &address,
            Vec::new(),
            &[("range", "bytes=100-199")],
        )
        .await;
    assert_eq!(status, StatusCode::PARTIAL_CONTENT);
    assert_eq!(headers["content-range"], "bytes 100-199/5000");
    assert_eq!(part, sound[100..200]);

    let (status, headers, _) = server
        .raw_with(
            &server.alex,
            "GET",
            &address,
            Vec::new(),
            &[("range", "bytes=6000-")],
        )
        .await;
    assert_eq!(status, StatusCode::RANGE_NOT_SATISFIABLE);
    assert_eq!(headers["content-range"], "bytes */5000");

    // The memo goes to the trash with its note.
    let (_, listed) = server.api(&server.alex, "GET", "/notes", None).await;
    let shown = listed["notes"]
        .as_array()
        .unwrap()
        .iter()
        .find(|note| note["id"] == id)
        .unwrap()["images"]
        .clone();
    assert_eq!(shown[0]["name"], "Trip 1.webm");
    server
        .api(&server.alex, "DELETE", &format!("/notes/{id}"), None)
        .await;
    assert!(
        server
            .storage
            .find(top, vec!["Trip 1.webm".into()])
            .await
            .unwrap()
            .is_none()
    );
}
