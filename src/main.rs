use router::router;
use std::env;
pub use types::*;

mod router;
mod types;

#[tokio::main]
async fn main() {
	env::set_var("RUST_BACKTRACE", "1");
	// build our application with a single route
	let app = router();

	let port = env::var("PORT").unwrap_or("8515".to_string());
	let address = format!("0.0.0.0:{}", port);
	println!("Listening on address {}", address);

	// run it with hyper on localhost:3000
	axum::Server::bind(&address.parse().unwrap()).serve(app.into_make_service()).await.unwrap();
}
