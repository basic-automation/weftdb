use api_v1::*;
use axum::{
	response::Json,
	routing::{get, post},
	Router,
};
use serde_json::{json, Value};
use sled::Db;

mod api_v1;

pub fn router() -> Router {
	let db = sled::open("db").unwrap();
	Router::new()
		.route("/", get(root))
		.route(
			"/destroy",
			get({
				let db = db.clone();
				move || destroy_db(db.clone())
			}),
		)
		.route(
			"/bucket",
			post(move |state, params, body| create_bucket(state, params, body))
		)
		.route(
			"/bucket/:bucket",
                        post(move |state, params, path, body| add_to_bucket(state, params, path, body))
                        .get({
				let db = db.clone();
				move |path, params| get_bucket(path, params, db.clone())
			})
			.delete({
				let db = db.clone();
				move |path, params| delete_bucket(path, params, db.clone())
			}),
		)
		.route(
			"/bucket/:bucket/:key",
			get({
				let db = db.clone();
				move |path, params| get_from_bucket(path, params, db.clone())
			})
			.delete({
				let db = db.clone();
				move |path, params| remove_from_bucket(path, params, db.clone())
			}),
		).with_state(db.clone())
}

async fn root() -> &'static str {
	"running"
}

async fn destroy_db(db: Db) -> Json<Value> {
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
