pub use api_v1::*;
use axum::{
	extract::State,
	response::Json,
	routing::{get, post},
	Router,
};
use serde_json::{json, Value};
use sled::Db;

pub mod api_v1;

pub fn router() -> Router {
	let db = sled::open("db").unwrap();
	Router::new()
        .route("/", 
                get(root))
        .route("/destroy", 
                get(destroy_db))
        .route("/bucket", 
                post(create_bucket))
        .route("/bucket/:bucket", 
                post(add_key_to_bucket)
                .get(move |state, path, params, body| get_bucket(path, params, state, body))
                .delete(move |state, path, params, body| delete_bucket(path, params, state, body)))
        .route("/bucket/:bucket/:key", 
        get(move |state, path, params, body| get_from_bucket(path, params, state, body))
                .delete(move |state, path, params, body| remove_from_bucket(path, params, state, body))
                .put(update_key))
        .with_state(db)
}

async fn root() -> &'static str {
	"running"
}

async fn destroy_db(State(db): State<Db>) -> Json<Value> {
	// loop through all the keys in the db and delete them
	let trees = db.tree_names();
	for tree in trees {
		let tree: String = String::from_utf8(tree.to_vec()).unwrap();
		if tree.as_str() != "__sled__default" {
			db.drop_tree(tree).unwrap();
		}
	}

	db.clear().unwrap();
	db.flush().unwrap();

	Json(json!({ "status": "ok" }))
}
