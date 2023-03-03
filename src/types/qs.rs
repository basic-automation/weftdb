use axum::{
	async_trait,
	extract::{FromRequest, FromRequestParts},
	http::{request::Parts, Request},
};
use std::convert::Infallible;

pub struct Qs<T>(pub T);

#[async_trait]
impl<S, B, T> FromRequest<S, B> for Qs<T>
where
	T: serde::de::DeserializeOwned,
	B: Send + 'static,
	S: Send + Sync,
{
	type Rejection = Infallible;

	async fn from_request(req: Request<B>, _state: &S) -> Result<Self, Self::Rejection> {
		// TODO: error handling
		let query = req.uri().query().unwrap();
		Ok(Self(serde_qs::from_str(query).unwrap()))
	}
}

#[async_trait]
impl<S, T> FromRequestParts<S> for Qs<T>
where
	T: serde::de::DeserializeOwned,
	S: Sized,
{
	type Rejection = Infallible;

	async fn from_request_parts(req: &mut Parts, _state: &S) -> Result<Self, Self::Rejection> {
		let query = match req.uri.query() {
			Some(query) => query,
			None => return Ok(Self(serde_qs::from_str("").unwrap())),
		};
		Ok(Self(serde_qs::from_str(query).unwrap()))
	}
}
