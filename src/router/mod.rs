use api_v1::*;
use axum::{routing::post, Router};

mod api_v1;

pub fn router() -> Router {
	Router::new().route("/sources/add", post(add_source))
}
