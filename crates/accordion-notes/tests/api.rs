//! Notes' API, mounted the way the server mounts it, with two people.

use std::time::Duration;

use accordion_core::coedit::{CoEditing, Timing};
use accordion_core::db::Database;
use accordion_core::events::{Actor, Events, Via};
use accordion_core::identity::{AuthMethod, Client, Identity, NewUser};
use accordion_core::live::Live;
use accordion_core::storage::{ItemKind, ShareLevel, Storage};
use accordion_core::texts::{self, Texts};
use accordion_core::thumbnails::{Thumbnails, Tools};
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
    let services = Services {
        db: db.clone(),
        coediting: CoEditing::open(db.clone(), storage.clone(), identity.clone(), QUICK)
            .await
            .unwrap(),
        identity: identity.clone(),
        storage: storage.clone(),
        events: Events::open(db.clone()).await.unwrap(),
        thumbnails,
        texts,
        live: Live::new(db, &identity),
        link_origin: watch::channel(None).1,
    };
    let (stop, stopping) = watch::channel(false);
    let started = Notes
        .start(services.clone(), Stopping::new(stopping))
        .await
        .unwrap();
    Server {
        router: Router::new().nest("/api/notes", started.routes.api.unwrap()),
        alex: person(&identity, "alex").await,
        sam: person(&identity, "sam").await,
        storage,
        services,
        _dir: dir,
        _stop: stop,
    }
}

async fn person(identity: &Identity, username: &str) -> Person {
    let user = identity
        .create_user(NewUser {
            username: username.into(),
            display_name: username.into(),
            role: Role::User,
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
async fn keeps_each_persons_list_settings() {
    let server = start().await;
    let (status, body) = server.api(&server.alex, "GET", "/settings", None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        body,
        json!({"new_items": "bottom", "checked_at_bottom": true})
    );

    let (status, _) = server
        .api(
            &server.alex,
            "PUT",
            "/settings",
            Some(json!({"new_items": "top", "checked_at_bottom": false})),
        )
        .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let (_, body) = server.api(&server.alex, "GET", "/settings", None).await;
    assert_eq!(
        body,
        json!({"new_items": "top", "checked_at_bottom": false})
    );

    // Sam's stay as they were.
    let (_, body) = server.api(&server.sam, "GET", "/settings", None).await;
    assert_eq!(
        body,
        json!({"new_items": "bottom", "checked_at_bottom": true})
    );

    let (status, _) = server
        .api(
            &server.alex,
            "PUT",
            "/settings",
            Some(json!({"new_items": "middle", "checked_at_bottom": true})),
        )
        .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
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
