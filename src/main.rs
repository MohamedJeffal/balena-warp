#![deny(warnings)]
#[macro_use] extern crate log;
extern crate pretty_env_logger;
extern crate serde;
#[macro_use] extern crate serde_derive;
extern crate warp;
extern crate hyper;

use std::env;
use std::sync::{Arc, Mutex};
use warp::{http::StatusCode, Filter};

use hyper::Client;
use hyper::rt::{Future, Stream};


/// So we don't have to tackle how different database work, we'll just use
/// a simple in-memory DB, a vector synchronized by a mutex.
type Db = Arc<Mutex<Vec<Todo>>>;

#[derive(Debug, Deserialize, Serialize)]
struct Todo {
    id: u64,
    text: String,
    completed: bool,
}

#[derive(Deserialize, Serialize, Debug)]
struct Post {
    #[serde(rename = "userId")]
    user_id: i32,
    id: i32,
    title: String,
    body: String,
}

/// Provides a RESTful web server managing some Todos.
///
/// API will be:
///
/// - `GET /todos`: return a JSON list of Todos.
/// - `POST /todos`: create a new Todo.
/// - `PUT /todos/:id`: update a specific Todo.
/// - `DELETE /todos/:id`: delete a specific Todo.
fn main() {
    if env::var_os("RUST_LOG").is_none() {
        // Set `RUST_LOG=todos=debug` to see debug logs,
        // this only shows access logs.
        env::set_var("RUST_LOG", "todos=info");
    }
    pretty_env_logger::init();

    // These are some `Filter`s that several of the endpoints share,
    // so we'll define them here and reuse them below...


    // Turn our "state", our db, into a Filter so we can combine it
    // easily with others...
    let db = Arc::new(Mutex::new(Vec::<Todo>::new()));
    let db = warp::any().map(move || db.clone());

    // Just the path segment "todos"...
    let todos = warp::path("todos");

    // Combined with `index`, this means nothing comes after "todos".
    // So, for example: `GET /todos`, but not `GET /todos/32`.
    let todos_index = todos.and(warp::path::end());

    // Combined with an id path parameter, for refering to a specific Todo.
    // For example, `POST /todos/32`, but not `POST /todos/32/something-more`.
    let todos_id = todos
        .and(warp::path::param::<u64>())
        .and(warp::path::end());

    // When accepting a body, we want a JSON body
    // (and to reject huge payloads)...
    let json_body = warp::body::content_length_limit(1024 * 16)
        .and(warp::body::json());

    // Next, we'll define each our 4 endpoints:

    // `GET /todos`
    let list = warp::get2()
        .and(todos_index)
        .and(db.clone())
        .map(list_todos);

    // `POST /todos`
    let create = warp::post2()
        .and(todos_index)
        .and(json_body)
        .and(db.clone())
        .and_then(create_todo);

    // `PUT /todos/:id`
    let update = warp::put2()
        .and(todos_id)
        .and(json_body)
        .and(db.clone())
        .and_then(update_todo);

    // `DELETE /todos/:id`
    let delete = warp::delete2()
        .and(todos_id)
        .and(db.clone())
        .and_then(delete_todo);
    
    let posts = warp::path("posts");
    let posts_index = posts.and(warp::path::end());

    let posts_list = warp::get2()
        .and(posts_index)
        .and_then(|| {
            debug!("list_posts");

            let posts_url = "http://jsonplaceholder.typicode.com/posts".parse().unwrap();

            fetch_json(posts_url)
                .map(|posts| warp::reply::json(&posts))
                .map_err(|_| warp::reject::not_found())
        });

    // Combine our endpoints, since we want requests to match any of them:
    let api = list
        .or(create)
        .or(update)
        .or(delete)
        .or(posts_list);

    // View access logs by setting `RUST_LOG=todos`.
    let routes = api.with(warp::log("todos"));

    // Start up the server...
    warp::serve(routes)
        .run(([127, 0, 0, 1], 8080));
}

// These are our API handlers, the ends of each filter chain.
// Notice how thanks to using `Filter::and`, we can define a function
// with the exact arguments we'd expect from each filter in the chain.
// No tuples are needed, it's auto flattened for the functions.

/// GET /todos
fn list_todos(db: Db) -> impl warp::Reply {
    // Just return a JSON array of all Todos.
    warp::reply::json(&*db.lock().unwrap())
}

fn fetch_json(url: hyper::Uri) -> impl Future<Item=Vec<Post>, Error=FetchError> {
    let client = Client::new();

    client
        // Fetch the url...
        .get(url)
        // And then, if we get a response back...
        .and_then(|res| {
            // asynchronously concatenate chunks of the body
            res.into_body().concat2()
        })
        .from_err::<FetchError>()
        // use the body after concatenation
        .and_then(|body| {
            // try to parse as json with serde_json
            let users = serde_json::from_slice(&body)?;

            Ok(users)
        })
        .from_err()
}

// Define a type so we can return multiple types of errors
enum FetchError {
    Http(hyper::Error),
    Json(serde_json::Error),
}

impl From<hyper::Error> for FetchError {
    fn from(err: hyper::Error) -> FetchError {
        FetchError::Http(err)
    }
}

impl From<serde_json::Error> for FetchError {
    fn from(err: serde_json::Error) -> FetchError {
        FetchError::Json(err)
    }
}

/// POST /todos with JSON body
fn create_todo(create: Todo, db: Db) -> Result<impl warp::Reply, warp::Rejection> {
    debug!("create_todo: {:?}", create);

    let mut vec = db
        .lock()
        .unwrap();

    for todo in vec.iter() {
        if todo.id == create.id {
            debug!("    -> id already exists: {}", create.id);
            // Todo with id already exists, return `400 BadRequest`.
            return Ok(StatusCode::BAD_REQUEST);
        }
    }

    // No existing Todo with id, so insert and return `201 Created`.
    vec.push(create);

    Ok(StatusCode::CREATED)
}

/// PUT /todos/:id with JSON body
fn update_todo(id: u64, update: Todo, db: Db) -> Result<impl warp::Reply, warp::Rejection> {
    debug!("update_todo: id={}, todo={:?}", id, update);
    let mut vec = db
        .lock()
        .unwrap();

    // Look for the specified Todo...
    for todo in vec.iter_mut() {
        if todo.id == id {
            *todo = update;
            return Ok(warp::reply());
        }
    }

    debug!("    -> todo id not found!");

    // If the for loop didn't return OK, then the ID doesn't exist...
    Err(warp::reject::not_found())
}

/// DELETE /todos/:id
fn delete_todo(id: u64, db: Db) -> Result<impl warp::Reply, warp::Rejection> {
    debug!("delete_todo: id={}", id);

    let mut vec = db
        .lock()
        .unwrap();

    let len = vec.len();
    vec.retain(|todo| {
        // Retain all Todos that aren't this id...
        // In other words, remove all that *are* this id...
        todo.id != id
    });

    // If the vec is smaller, we found and deleted a Todo!
    let deleted = vec.len() != len;

    if deleted {
        // respond with a `204 No Content`, which means successful,
        // yet no body expected...
        Ok(StatusCode::NO_CONTENT)
    } else {
        debug!("    -> todo id not found!");
        // Reject this request with a `404 Not Found`...
        Err(warp::reject::not_found())
    }
}
