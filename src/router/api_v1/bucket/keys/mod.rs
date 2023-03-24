use super::*;
pub use add::*;
use axum::Json;
pub use get::*;
pub use remove::*;
use serde_json::json;
use serde_json::Value;
use sled::Db;
pub use update::*;

mod add;
pub mod get;
mod remove;
mod update;

pub(crate) async fn err_debug(err: &str, db: Db, debug: bool) -> Json<Value> {
	if debug {
		let debugdb = DebugDb { db };
		Json(json!({ "error": err, "db": debugdb }))
	} else {
		Json(json!({ "error": err }))
	}
}
