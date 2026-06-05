use crate::types::*;
use axum::body::Bytes;
use axum::{extract::Path, extract::State};
pub use buckets::*;
pub use keys::*;
use serde::{ser::SerializeStruct, Deserialize, Serialize};
use serde_json::{json, Value};
use sled::Db;
use std::collections::HashMap;

mod buckets;
pub mod keys;

pub async fn process_tags(tags: Option<Vec<String>>) -> Option<HashMap<String, String>> {
	match tags {
		Some(tags) => {
			let mut res = HashMap::new();
			for tag in tags {
				if !tag.contains('=') {
					res.insert(tag, "".to_string());
				} else {
					let tag: Vec<&str> = tag.split('=').collect();
					res.insert(tag[0].to_string(), tag[1].to_string());
				}
			}
			Some(res)
		}
		None => None,
	}
}

pub struct DebugDb {
	db: Db,
}

impl Serialize for DebugDb {
	fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
	where
		S: serde::Serializer,
	{
		let mut state = serializer.serialize_struct("debug", 1)?;
		let tree_names = self.db.tree_names();
		let mut trees: Vec<Value> = Vec::new();
		for name in tree_names {
			let name = std::str::from_utf8(name.as_ref()).unwrap();
			let tree = self.db.open_tree(name).unwrap();
			let mut items: Vec<Value> = Vec::new();
			for item in tree.iter() {
				let item = item.unwrap();
				let key = std::str::from_utf8(item.0.as_ref()).unwrap_or("unknown value");
				let value: Value = serde_json::from_str(std::str::from_utf8(item.1.as_ref()).unwrap_or("unknown value")).unwrap();
				items.push(json!({ key: value }));
			}
			trees.push(json!({ name: items }));
		}
		state.serialize_field("db", &trees)?;
		state.end()
	}
}
