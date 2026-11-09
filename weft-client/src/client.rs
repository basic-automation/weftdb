//! [`Client`]: one method per route of weft-server's HTTP API.

use std::{fmt, time::Duration};

use futures::{stream, Stream, TryStreamExt as _};
use reqwest::{header, Method, StatusCode, Url};
use serde::{de::DeserializeOwned, Serialize};
use sha2::{Digest as _, Sha256};

use crate::{
	error::{Error, ErrorKind, Result}, types::{AspectDeclaration, AspectEnvelope, AspectInfo, AspectList, AspectStats, BenchRun, BenchRunList, ComputePoint, Downsample, Downsampled, Format, Headroom, IlpOptions, Interpolate, InterpolatePointBody, Interpolated, InterpolatedPoint, Page, PointAt, PointsAt, PointsBody, PointsPage, Spline, StoreStats, StoredDownsample, StoredPoint, StoredValue, WireStoredPoint, WriteOptions, WriteSummary}
};

const PARQUET: &str = "application/vnd.apache.parquet";
const JSON: &str = "application/json";

/// The longest `Retry-After` the client will sleep through before retrying a read.
const MAX_RETRY_WAIT: Duration = Duration::from_secs(30);

/// Configures a [`Client`].
#[derive(Clone)]
#[must_use]
pub struct ClientBuilder {
	base_url: String,
	api_key: Option<String>,
	timeout: Duration,
	connect_timeout: Duration,
	max_retries: u32,
	user_agent: Option<String>,
	#[cfg(feature = "rustls")]
	root_pem: Option<Vec<u8>>,
}

impl fmt::Debug for ClientBuilder {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		f.debug_struct("ClientBuilder").field("base_url", &self.base_url).field("api_key", &self.api_key.as_ref().map(|_| "<set>")).field("timeout", &self.timeout).field("max_retries", &self.max_retries).finish_non_exhaustive()
	}
}

impl ClientBuilder {
	/// A key from `weft-server keygen`, sent as a Bearer token. Never put it in the URL: the
	/// server refuses a key in a query string.
	pub fn api_key(mut self, key: impl Into<String>) -> Self {
		self.api_key = Some(key.into());
		self
	}

	/// The whole-request timeout (default 30 s).
	pub const fn timeout(mut self, timeout: Duration) -> Self {
		self.timeout = timeout;
		self
	}

	/// The connect timeout (default 10 s).
	pub const fn connect_timeout(mut self, timeout: Duration) -> Self {
		self.connect_timeout = timeout;
		self
	}

	/// How many times a read refused `503 overloaded` is retried after the server's
	/// `Retry-After` (default 2). Writes are never retried by the client: send them with
	/// an idempotency key and retry them yourself.
	pub const fn max_retries(mut self, retries: u32) -> Self {
		self.max_retries = retries;
		self
	}

	/// The `User-Agent` (default `weft-client/<version>`).
	pub fn user_agent(mut self, agent: impl Into<String>) -> Self {
		self.user_agent = Some(agent.into());
		self
	}

	/// Trust only the certificates in `pem` (one or more `CERTIFICATE` blocks) instead of
	/// the platform's roots — for a server whose certificate a private CA signed, or one
	/// `weft-server` generated for itself.
	#[cfg(feature = "rustls")]
	pub fn root_certificates_pem(mut self, pem: impl Into<Vec<u8>>) -> Self {
		self.root_pem = Some(pem.into());
		self
	}

	/// The client.
	///
	/// # Errors
	///
	/// [`ErrorKind::InvalidInput`] for a base URL that is not an `http(s)` origin, that
	/// carries a query, or that is `https` without the `rustls` feature.
	pub fn build(self) -> Result<Client> {
		let base = Url::parse(&self.base_url).map_err(|error| Error::invalid(format!("{:?} is not a URL: {error}", self.base_url)))?;
		if !matches!(base.scheme(), "http" | "https") || base.host_str().is_none() {
			return Err(Error::invalid(format!("{:?} is not an http(s) origin", self.base_url)));
		}
		if base.query().is_some() || !base.username().is_empty() || base.password().is_some() {
			return Err(Error::invalid("the base URL carries no query and no credentials; a key goes in `api_key`, never the URL"));
		}
		let agent = self.user_agent.unwrap_or_else(|| format!("weft-client/{}", env!("CARGO_PKG_VERSION")));
		let builder = reqwest::Client::builder().timeout(self.timeout).connect_timeout(self.connect_timeout).user_agent(agent).redirect(reqwest::redirect::Policy::none());
		#[cfg(feature = "rustls")]
		let builder = builder.tls_backend_preconfigured(tls_config(self.root_pem.as_deref())?);
		#[cfg(not(feature = "rustls"))]
		if base.scheme() == "https" {
			return Err(Error::invalid("an https server needs weftdb-client's `rustls` feature"));
		}
		let http = builder.build().map_err(|error| Error::new(ErrorKind::InvalidInput, format!("cannot build the HTTP client: {error}")).with_source(error))?;
		Ok(Client { http, base, api_key: self.api_key, max_retries: self.max_retries })
	}
}

#[cfg(feature = "rustls")]
fn tls_config(root_pem: Option<&[u8]>) -> Result<rustls::ClientConfig> {
	use std::sync::Arc;

	use rustls::pki_types::{pem::PemObject as _, CertificateDer};
	use rustls_platform_verifier::BuilderVerifierExt as _;

	let tls_error = |error: rustls::Error| Error::invalid(format!("TLS setup: {error}")).with_source(error);
	let builder = rustls::ClientConfig::builder_with_provider(Arc::new(rustls::crypto::ring::default_provider())).with_safe_default_protocol_versions().map_err(tls_error)?;
	let mut config = match root_pem {
		Some(pem) => {
			let mut roots = rustls::RootCertStore::empty();
			for cert in CertificateDer::pem_slice_iter(pem) {
				let cert = cert.map_err(|error| Error::invalid(format!("root certificates: {error}")))?;
				roots.add(cert).map_err(tls_error)?;
			}
			if roots.is_empty() {
				return Err(Error::invalid("root certificates: no CERTIFICATE block in the PEM"));
			}
			builder.with_root_certificates(roots).with_no_client_auth()
		}
		None => builder.with_platform_verifier().map_err(tls_error)?.with_no_client_auth(),
	};
	config.alpn_protocols = vec![b"http/1.1".to_vec()];
	Ok(config)
}

/// A connection to one `weft-server`.
///
/// Cheap to clone (the connection pool is shared). Every method is one request to one
/// route, named for what it does; the two `*_stream` methods follow stored-read cursors.
#[derive(Clone)]
pub struct Client {
	http: reqwest::Client,
	base: Url,
	api_key: Option<String>,
	max_retries: u32,
}

impl fmt::Debug for Client {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		f.debug_struct("Client").field("base", &self.base.as_str()).field("api_key", &self.api_key.as_ref().map(|_| "<set>")).finish_non_exhaustive()
	}
}

/// A successful answer: its status, headers and body.
#[derive(Debug, Clone)]
pub struct Answer {
	/// The status.
	pub status: StatusCode,
	/// The headers.
	pub headers: header::HeaderMap,
	/// The body.
	pub body: Vec<u8>,
}

impl Answer {
	fn json<T: DeserializeOwned>(&self) -> Result<T> {
		serde_json::from_slice(&self.body).map_err(|error| Error::new(ErrorKind::Decode, format!("cannot decode the answer: {error}")).with_source(error))
	}

	fn text(self) -> Result<String> {
		String::from_utf8(self.body).map_err(|error| Error::new(ErrorKind::Decode, "the answer is not UTF-8").with_source(error))
	}
}

/// One request, before it is sent.
struct Call<'a> {
	method: Method,
	path: Vec<&'a str>,
	query: Vec<(&'static str, String)>,
	body: Option<(Vec<u8>, &'static str)>,
	idempotency_key: Option<&'a str>,
	accept: &'a [u16],
}

impl<'a> Call<'a> {
	fn get(path: &[&'a str]) -> Self {
		Self { method: Method::GET, path: path.to_vec(), query: Vec::new(), body: None, idempotency_key: None, accept: &[] }
	}

	fn post(path: &[&'a str]) -> Self {
		Self { method: Method::POST, ..Self::get(path) }
	}

	fn param(mut self, key: &'static str, value: Option<impl ToString>) -> Self {
		if let Some(value) = value {
			self.query.push((key, value.to_string()));
		}
		self
	}

	fn json(mut self, payload: &impl Serialize) -> Result<Self> {
		let bytes = serde_json::to_vec(payload).map_err(|error| Error::invalid(format!("cannot encode the request: {error}")).with_source(error))?;
		self.body = Some((bytes, JSON));
		Ok(self)
	}

	fn bytes(mut self, body: Vec<u8>, content_type: &'static str) -> Self {
		self.body = Some((body, content_type));
		self
	}

	fn write_options(self, options: &'a WriteOptions) -> Self {
		let mut call = self.param("rows_per_page", options.rows_per_page).param("require_sorted", options.require_sorted.then_some("true"));
		call.idempotency_key = options.idempotency_key.as_deref();
		call
	}
}

fn format_path(base: &str, format: Format, separator: char, json_suffix: bool) -> String {
	match format {
		Format::Json if !json_suffix => base.to_owned(),
		_ => format!("{base}{separator}{}", format.name()),
	}
}

fn agg(aggregations: &[String]) -> Option<String> {
	(!aggregations.is_empty()).then(|| aggregations.join(","))
}

fn decimal_param(value: &impl StoredValue, which: &str) -> Result<String> {
	value.to_decimal_text()?.ok_or_else(|| Error::invalid(format!("`{which}` bounds a value band and cannot be null")))
}

fn spline_param(spline: Option<Spline>) -> Result<Option<&'static str>> {
	spline.map(|spline| spline.query_token().ok_or_else(|| Error::invalid("the Line Protocol endpoints take `linear`, `quadratic` or `cubic`; a polynomial spline needs the JSON body"))).transpose()
}

/// The window a stored read is over.
#[derive(Clone)]
enum Window {
	Time(i64, i64),
	Value(String, String),
}

impl Client {
	/// A client for the server at `base_url` (e.g. `http://127.0.0.1:8080`) with the defaults.
	///
	/// # Errors
	///
	/// As [`ClientBuilder::build`].
	pub fn new(base_url: impl Into<String>) -> Result<Self> {
		Self::builder(base_url).build()
	}

	/// A builder for the server at `base_url`.
	pub fn builder(base_url: impl Into<String>) -> ClientBuilder {
		ClientBuilder {
			base_url: base_url.into(),
			api_key: None,
			timeout: Duration::from_secs(30),
			connect_timeout: Duration::from_secs(10),
			max_retries: 2,
			user_agent: None,
			#[cfg(feature = "rustls")]
			root_pem: None,
		}
	}

	/// The server's origin.
	#[must_use]
	pub fn base_url(&self) -> &str {
		self.base.as_str()
	}

	fn url(&self, path: &[&str], query: &[(&'static str, String)]) -> Url {
		let mut url = self.base.clone();
		url.path_segments_mut().expect("an http(s) URL has a path").pop_if_empty().extend(path);
		if !query.is_empty() {
			url.query_pairs_mut().extend_pairs(query.iter().map(|(key, value)| (*key, value.as_str())));
		}
		url
	}

	async fn send(&self, call: Call<'_>) -> Result<Answer> {
		// A segment of `.` or `..` would be resolved away by the URL rather than sent as a name.
		if let Some(segment) = call.path.iter().find(|segment| matches!(**segment, "" | "." | "..")) {
			return Err(Error::invalid(format!("{segment:?} is not a name the server can address")));
		}
		let url = self.url(&call.path, &call.query);
		let mut attempt = 0;
		loop {
			let mut request = self.http.request(call.method.clone(), url.clone()).header(header::ACCEPT, "*/*");
			if let Some(key) = &self.api_key {
				request = request.bearer_auth(key);
			}
			if let Some(key) = call.idempotency_key {
				request = request.header("Idempotency-Key", key);
			}
			if let Some((body, content_type)) = &call.body {
				request = request.header(header::CONTENT_TYPE, *content_type).body(body.clone());
			}
			let response = request.send().await.map_err(|error| Error::new(ErrorKind::Transport, format!("cannot reach {}: {error}", self.base)).with_source(error))?;
			let status = response.status();
			let headers = response.headers().clone();
			let body = response.bytes().await.map_err(|error| Error::new(ErrorKind::Transport, format!("the answer from {} was cut off: {error}", self.base)).with_source(error))?.to_vec();
			if status.is_success() || call.accept.contains(&status.as_u16()) {
				return Ok(Answer { status, headers, body });
			}
			let request_id = headers.get("x-request-id").and_then(|value| value.to_str().ok()).map(str::to_owned);
			let retry_after = headers.get(header::RETRY_AFTER).and_then(|value| value.to_str().ok()).and_then(|value| value.trim().parse::<f64>().ok()).and_then(|seconds| Duration::try_from_secs_f64(seconds).ok());
			let error = Error::refusal(status.as_u16(), status.canonical_reason().unwrap_or("refused"), request_id, retry_after, &body);
			if error.kind() == ErrorKind::Overloaded && call.method == Method::GET && attempt < self.max_retries {
				attempt += 1;
				tokio::time::sleep(retry_after.unwrap_or(Duration::from_secs(1)).min(MAX_RETRY_WAIT)).await;
				continue;
			}
			return Err(error);
		}
	}

	/// Send any request to the server and get its answer, or the refusal as an [`Error`].
	/// `path` is the route's segments, each percent-encoded as one segment.
	///
	/// # Errors
	///
	/// The server's refusal, or a transport failure.
	pub async fn raw(&self, method: Method, path: &[&str], query: &[(&'static str, String)], body: Option<(Vec<u8>, &'static str)>) -> Result<Answer> {
		self.send(Call { method, path: path.to_vec(), query: query.to_vec(), body, idempotency_key: None, accept: &[] }).await
	}

	// ── service ──────────────────────────────────────────────────────────────

	/// `GET /health`: the process is up.
	///
	/// # Errors
	///
	/// The server's refusal, or a transport failure.
	pub async fn health(&self) -> Result<serde_json::Value> {
		self.send(Call::get(&["health"])).await?.json()
	}

	/// `GET /ready`: whether the server can serve, with its store's identity and health. A
	/// `503` here is still the readiness document (`"ready": false`), so it is returned.
	///
	/// # Errors
	///
	/// The server's refusal, or a transport failure.
	pub async fn ready(&self) -> Result<serde_json::Value> {
		self.send(Call { accept: &[503], ..Call::get(&["ready"]) }).await?.json()
	}

	/// `GET /metrics`: the Prometheus text exposition (`metrics` scope).
	///
	/// # Errors
	///
	/// The server's refusal, or a transport failure.
	pub async fn metrics(&self) -> Result<String> {
		self.send(Call::get(&["metrics"])).await?.text()
	}

	/// `GET /debug/profile/current`: live p50/p95/p99 per instrumented path (`metrics` scope).
	///
	/// # Errors
	///
	/// The server's refusal, or a transport failure.
	pub async fn profile(&self) -> Result<serde_json::Value> {
		self.send(Call::get(&["debug", "profile", "current"])).await?.json()
	}

	/// `GET /bench/runs`: the benchmark runs the server holds, newest first (experimental).
	///
	/// # Errors
	///
	/// The server's refusal (`503 unconfigured` without `WEFT_BENCH_RUNS_DIR`), or a transport failure.
	pub async fn bench_runs(&self) -> Result<Vec<BenchRun>> {
		Ok(self.send(Call::get(&["bench", "runs"])).await?.json::<BenchRunList>()?.runs)
	}

	/// `GET /bench/runs/{id}`: one run's manifest, byte for byte (experimental).
	///
	/// The id is the SHA-256 of those bytes, so the answer is checked: bytes that do not hash
	/// to `run_id` are an [`ErrorKind::Integrity`] error, never returned as that run.
	///
	/// # Errors
	///
	/// The server's refusal (`404 run_not_found`), a transport failure, or a hash mismatch.
	pub async fn bench_run(&self, run_id: &str) -> Result<Vec<u8>> {
		let answer = self.send(Call::get(&["bench", "runs", run_id])).await?;
		let actual = format!("{:x}", Sha256::digest(&answer.body));
		if actual != run_id {
			return Err(Error::new(ErrorKind::Integrity, format!("the server answered run {run_id} with bytes that hash to {actual}")));
		}
		Ok(answer.body)
	}

	// ── compute ──────────────────────────────────────────────────────────────

	/// `POST /api/v1/interpolate`: reconstruct a series on a regular grid.
	///
	/// # Errors
	///
	/// The server's refusal (`result_too_large` names its `limit`), or a transport failure.
	pub async fn interpolate(&self, request: &Interpolate) -> Result<Interpolated> {
		self.send(Call::post(&["api", "v1", "interpolate"]).json(request)?).await?.json()
	}

	/// [`interpolate`](Self::interpolate) answered as CSV, Arrow or Parquet bytes.
	///
	/// # Errors
	///
	/// As [`interpolate`](Self::interpolate).
	pub async fn interpolate_as(&self, request: &Interpolate, format: Format) -> Result<Vec<u8>> {
		let last = format.name();
		let path: &[&str] = if format == Format::Json { &["api", "v1", "interpolate"] } else { &["api", "v1", "interpolate", last] };
		Ok(self.send(Call::post(path).json(request)?).await?.body)
	}

	/// `POST /api/v1/interpolate/point`: the reconstructed signal at one instant.
	///
	/// # Errors
	///
	/// The server's refusal, or a transport failure.
	pub async fn interpolate_point(&self, points: &[ComputePoint], instant: chrono::DateTime<chrono::Utc>, spline: Spline) -> Result<InterpolatedPoint> {
		self.send(Call::post(&["api", "v1", "interpolate", "point"]).json(&InterpolatePointBody { spline, instant, points })?).await?.json()
	}

	/// `POST /api/v1/downsample`: reduce a series into epoch-aligned buckets.
	///
	/// # Errors
	///
	/// The server's refusal, or a transport failure.
	pub async fn downsample(&self, request: &Downsample) -> Result<Downsampled> {
		self.send(Call::post(&["api", "v1", "downsample"]).json(request)?).await?.json()
	}

	/// [`downsample`](Self::downsample) answered as CSV, Arrow or Parquet bytes.
	///
	/// # Errors
	///
	/// As [`downsample`](Self::downsample).
	pub async fn downsample_as(&self, request: &Downsample, format: Format) -> Result<Vec<u8>> {
		let last = format.name();
		let path: &[&str] = if format == Format::Json { &["api", "v1", "downsample"] } else { &["api", "v1", "downsample", last] };
		Ok(self.send(Call::post(path).json(request)?).await?.body)
	}

	fn ilp_call<'a>(path: &[&'a str], body: impl Into<Vec<u8>>, options: &IlpOptions, interpolate: bool) -> Result<Call<'a>> {
		if options.field.is_empty() {
			return Err(Error::invalid("the Line Protocol endpoints need the `field` to read"));
		}
		let mut call = Call::post(path).param("field", Some(&options.field)).param("precision", options.precision.as_ref()).param("resolution", options.resolution.as_ref());
		if interpolate {
			call = call.param("spline", spline_param(options.spline)?);
		} else {
			call = call.param("agg", agg(&options.aggregations)).param("fill", options.fill.as_ref());
		}
		Ok(call.bytes(body.into(), "text/plain"))
	}

	/// `POST /api/v1/interpolate/ilp`: interpolate one field of an InfluxDB Line Protocol body.
	///
	/// # Errors
	///
	/// The server's refusal, or a transport failure.
	pub async fn interpolate_ilp(&self, body: impl Into<Vec<u8>>, options: &IlpOptions) -> Result<Interpolated> {
		self.send(Self::ilp_call(&["api", "v1", "interpolate", "ilp"], body, options, true)?).await?.json()
	}

	/// [`interpolate_ilp`](Self::interpolate_ilp) answered as CSV, Arrow or Parquet bytes.
	///
	/// # Errors
	///
	/// As [`interpolate_ilp`](Self::interpolate_ilp).
	pub async fn interpolate_ilp_as(&self, body: impl Into<Vec<u8>>, options: &IlpOptions, format: Format) -> Result<Vec<u8>> {
		let last = format.name();
		let path: &[&str] = if format == Format::Json { &["api", "v1", "interpolate", "ilp"] } else { &["api", "v1", "interpolate", "ilp", last] };
		Ok(self.send(Self::ilp_call(path, body, options, true)?).await?.body)
	}

	/// `POST /api/v1/downsample/ilp`: downsample one field of a Line Protocol body.
	///
	/// # Errors
	///
	/// The server's refusal, or a transport failure.
	pub async fn downsample_ilp(&self, body: impl Into<Vec<u8>>, options: &IlpOptions) -> Result<Downsampled> {
		self.send(Self::ilp_call(&["api", "v1", "downsample", "ilp"], body, options, false)?).await?.json()
	}

	/// [`downsample_ilp`](Self::downsample_ilp) answered as CSV, Arrow or Parquet bytes.
	///
	/// # Errors
	///
	/// As [`downsample_ilp`](Self::downsample_ilp).
	pub async fn downsample_ilp_as(&self, body: impl Into<Vec<u8>>, options: &IlpOptions, format: Format) -> Result<Vec<u8>> {
		let last = format.name();
		let path: &[&str] = if format == Format::Json { &["api", "v1", "downsample", "ilp"] } else { &["api", "v1", "downsample", "ilp", last] };
		Ok(self.send(Self::ilp_call(path, body, options, false)?).await?.body)
	}

	// ── catalog ──────────────────────────────────────────────────────────────

	/// `POST /api/v1/storage/aspects`: declare an aspect. Declaring an identical schema again
	/// is a no-op; a different one is `409`.
	///
	/// # Errors
	///
	/// The server's refusal, or a transport failure.
	pub async fn declare_aspect(&self, declaration: &AspectDeclaration) -> Result<AspectInfo> {
		Ok(self.send(Call::post(&["api", "v1", "storage", "aspects"]).json(declaration)?).await?.json::<AspectEnvelope>()?.aspect)
	}

	/// `GET /api/v1/storage/aspects`: every declared schema.
	///
	/// # Errors
	///
	/// The server's refusal, or a transport failure.
	pub async fn aspects(&self) -> Result<Vec<AspectInfo>> {
		Ok(self.send(Call::get(&["api", "v1", "storage", "aspects"])).await?.json::<AspectList>()?.aspects)
	}

	/// `GET /api/v1/storage/{aspect}/schema`.
	///
	/// # Errors
	///
	/// The server's refusal (`404 aspect_not_found`), or a transport failure.
	pub async fn aspect_schema(&self, aspect: &str) -> Result<AspectInfo> {
		Ok(self.send(Call::get(&["api", "v1", "storage", aspect, "schema"])).await?.json::<AspectEnvelope>()?.aspect)
	}

	/// `GET /api/v1/storage/catalog`: the store's scope and registered hierarchy.
	///
	/// # Errors
	///
	/// The server's refusal, or a transport failure.
	pub async fn catalog(&self) -> Result<serde_json::Value> {
		self.send(Call::get(&["api", "v1", "storage", "catalog"])).await?.json()
	}

	/// `GET /api/v1/storage/stats`: the store's totals.
	///
	/// # Errors
	///
	/// The server's refusal, or a transport failure.
	pub async fn stats(&self) -> Result<StoreStats> {
		self.send(Call::get(&["api", "v1", "storage", "stats"])).await?.json()
	}

	/// `GET /api/v1/storage/headroom`: the store's free-space margin (ROB-17,
	/// experimental, `metrics` scope).
	///
	/// # Errors
	///
	/// The server's refusal, or a transport failure.
	pub async fn headroom(&self) -> Result<Headroom> {
		self.send(Call::get(&["api", "v1", "storage", "headroom"])).await?.json()
	}

	/// `GET /api/v1/storage/{aspect}/stats`: one aspect's totals.
	///
	/// # Errors
	///
	/// The server's refusal, or a transport failure.
	pub async fn aspect_stats(&self, aspect: &str) -> Result<AspectStats> {
		self.send(Call::get(&["api", "v1", "storage", aspect, "stats"])).await?.json()
	}

	// ── ingest ───────────────────────────────────────────────────────────────

	/// `POST /api/v1/storage/{aspect}/points`: seal a batch of `(timestamp, value)` rows.
	///
	/// Timestamps are integers in the aspect's declared unit; values are any
	/// [`StoredValue`], `None` for a null.
	///
	/// # Errors
	///
	/// [`ErrorKind::InvalidInput`] for a value that is not a decimal; the server's refusal
	/// (`value_not_representable` for a value the aspect's encoding cannot hold); or a
	/// transport failure.
	pub async fn write_points<V: StoredValue>(&self, aspect: &str, points: impl IntoIterator<Item = (i64, V)>, options: &WriteOptions) -> Result<WriteSummary> {
		let points = points.into_iter().map(|(timestamp, value)| Ok(WireStoredPoint { timestamp, value: value.to_decimal_text()? })).collect::<Result<Vec<_>>>()?;
		let body = PointsBody { points, require_sorted: options.require_sorted, rows_per_page: options.rows_per_page };
		let mut call = Call::post(&["api", "v1", "storage", aspect, "points"]).json(&body)?;
		call.idempotency_key = options.idempotency_key.as_deref();
		self.send(call).await?.json()
	}

	/// `POST /api/v1/storage/{aspect}/ilp`: seal one field of a Line Protocol body
	/// (`options.precision` is the body's timestamp precision, `options.measurement` the
	/// measurement sealed, which the server requires).
	///
	/// # Errors
	///
	/// The server's refusal, or a transport failure.
	pub async fn write_ilp(&self, aspect: &str, field: &str, body: impl Into<Vec<u8>>, options: &WriteOptions) -> Result<WriteSummary> {
		let call = Call::post(&["api", "v1", "storage", aspect, "ilp"]).param("measurement", options.measurement.as_ref()).param("field", Some(field)).param("precision", options.precision.as_ref()).write_options(options).bytes(body.into(), "text/plain");
		self.send(call).await?.json()
	}

	/// `POST /api/v1/storage/{aspect}/csv`: seal `timestamp,value` CSV rows.
	///
	/// # Errors
	///
	/// The server's refusal, or a transport failure.
	pub async fn write_csv(&self, aspect: &str, csv: impl Into<Vec<u8>>, options: &WriteOptions) -> Result<WriteSummary> {
		self.send(Call::post(&["api", "v1", "storage", aspect, "csv"]).write_options(options).bytes(csv.into(), "text/csv")).await?.json()
	}

	/// `POST /api/v1/storage/{aspect}/parquet`: seal a Parquet file.
	///
	/// # Errors
	///
	/// The server's refusal, or a transport failure.
	pub async fn write_parquet(&self, aspect: &str, parquet: impl Into<Vec<u8>>, options: &WriteOptions) -> Result<WriteSummary> {
		self.send(Call::post(&["api", "v1", "storage", aspect, "parquet"]).write_options(options).bytes(parquet.into(), PARQUET)).await?.json()
	}

	// ── stored reads ─────────────────────────────────────────────────────────

	async fn page(&self, aspect: &str, window: &Window, page: &Page) -> Result<PointsPage> {
		let call = match window {
			Window::Time(start, end) => Call::get(&["api", "v1", "storage", aspect, "points"]).param("start", Some(start)).param("end", Some(end)),
			Window::Value(lo, hi) => Call::get(&["api", "v1", "storage", aspect, "value-points"]).param("lo", Some(lo)).param("hi", Some(hi)),
		};
		let call = call.param("limit", page.limit).param("offset", page.offset).param("cursor", page.cursor.as_ref());
		self.send(call).await?.json()
	}

	fn follow<'a>(&'a self, aspect: &'a str, window: Window, page_size: usize) -> impl Stream<Item = Result<StoredPoint>> + 'a {
		let size = page_size.max(1);
		stream::try_unfold(Some(Page::first(size)), move |next| {
			let window = window.clone();
			async move {
				let Some(page) = next else { return Ok(None) };
				let answer = self.page(aspect, &window, &page).await?;
				let following = answer.next_cursor.map(|cursor| Page::after(cursor, size));
				Ok(Some((stream::iter(answer.points.into_iter().map(Ok)), following)))
			}
		})
		.try_flatten()
	}

	/// `GET /api/v1/storage/{aspect}/points`: one page of the stored window `[start, end]`.
	///
	/// # Errors
	///
	/// The server's refusal, or a transport failure.
	pub async fn points(&self, aspect: &str, start: i64, end: i64, page: &Page) -> Result<PointsPage> {
		self.page(aspect, &Window::Time(start, end), page).await
	}

	/// Every stored row in `[start, end]`, fetched `page_size` at a time by following
	/// `next_cursor`.
	pub fn points_stream<'a>(&'a self, aspect: &'a str, start: i64, end: i64, page_size: usize) -> impl Stream<Item = Result<StoredPoint>> + 'a {
		self.follow(aspect, Window::Time(start, end), page_size)
	}

	/// `GET /api/v1/storage/{aspect}/value-points`: one page of the rows whose value is in
	/// `[lo, hi]`.
	///
	/// # Errors
	///
	/// [`ErrorKind::InvalidInput`] for a bound that is not a decimal, the server's refusal,
	/// or a transport failure.
	pub async fn value_points(&self, aspect: &str, lo: impl StoredValue, hi: impl StoredValue, page: &Page) -> Result<PointsPage> {
		self.page(aspect, &Window::Value(decimal_param(&lo, "lo")?, decimal_param(&hi, "hi")?), page).await
	}

	/// Every stored row whose value is in `[lo, hi]`, following `next_cursor`.
	///
	/// # Errors
	///
	/// [`ErrorKind::InvalidInput`] for a bound that is not a decimal; read failures arrive
	/// on the stream.
	pub fn value_points_stream<'a>(&'a self, aspect: &'a str, lo: impl StoredValue, hi: impl StoredValue, page_size: usize) -> Result<impl Stream<Item = Result<StoredPoint>> + 'a> {
		Ok(self.follow(aspect, Window::Value(decimal_param(&lo, "lo")?, decimal_param(&hi, "hi")?), page_size))
	}

	/// `GET /api/v1/storage/{aspect}/at`: the stored value at exactly `t`.
	///
	/// # Errors
	///
	/// The server's refusal, or a transport failure.
	pub async fn at(&self, aspect: &str, t: i64) -> Result<PointAt> {
		self.send(Call::get(&["api", "v1", "storage", aspect, "at"]).param("t", Some(t))).await?.json()
	}

	/// `GET /api/v1/storage/{aspect}/at-multi`: many instants in one pass, in query order.
	///
	/// # Errors
	///
	/// The server's refusal, or a transport failure.
	pub async fn at_multi(&self, aspect: &str, instants: &[i64]) -> Result<Vec<PointAt>> {
		let joined = instants.iter().map(i64::to_string).collect::<Vec<_>>().join(",");
		Ok(self.send(Call::get(&["api", "v1", "storage", aspect, "at-multi"]).param("t", Some(joined))).await?.json::<PointsAt>()?.points)
	}

	fn columnar(format: Format, route: &str) -> Result<String> {
		match format {
			Format::Arrow => Ok(route.to_owned()),
			Format::Csv | Format::Parquet => Ok(format_path(route, format, '.', true)),
			_ => Err(Error::invalid(format!("the stored {route} answers Arrow, Parquet or CSV; the JSON rows are `points`/`value_points`"))),
		}
	}

	/// The stored window `[start, end]` as Arrow IPC (`…/range`), Parquet or CSV bytes.
	///
	/// # Errors
	///
	/// [`ErrorKind::InvalidInput`] for [`Format::Json`], the server's refusal, or a transport failure.
	pub async fn range(&self, aspect: &str, start: i64, end: i64, format: Format) -> Result<Vec<u8>> {
		let last = Self::columnar(format, "range")?;
		Ok(self.send(Call::get(&["api", "v1", "storage", aspect, &last]).param("start", Some(start)).param("end", Some(end))).await?.body)
	}

	/// The stored value band `[lo, hi]` as Arrow IPC (`…/value-range`), Parquet or CSV bytes.
	///
	/// # Errors
	///
	/// As [`range`](Self::range), and [`ErrorKind::InvalidInput`] for a bound that is not a decimal.
	pub async fn value_range(&self, aspect: &str, lo: impl StoredValue, hi: impl StoredValue, format: Format) -> Result<Vec<u8>> {
		let last = Self::columnar(format, "value-range")?;
		let call = Call::get(&["api", "v1", "storage", aspect, &last]).param("lo", Some(decimal_param(&lo, "lo")?)).param("hi", Some(decimal_param(&hi, "hi")?));
		Ok(self.send(call).await?.body)
	}

	fn stored_downsample_call<'a>(path: &[&'a str], query: &StoredDownsample) -> Call<'a> {
		Call::get(path).param("start", query.start).param("end", query.end).param("resolution", query.resolution.as_ref()).param("agg", agg(&query.aggregations)).param("fill", query.fill.as_ref())
	}

	/// `GET /api/v1/storage/{aspect}/downsample`: reduce stored segments in place.
	///
	/// # Errors
	///
	/// The server's refusal, or a transport failure.
	pub async fn stored_downsample(&self, aspect: &str, query: &StoredDownsample) -> Result<Downsampled> {
		self.send(Self::stored_downsample_call(&["api", "v1", "storage", aspect, "downsample"], query)).await?.json()
	}

	/// [`stored_downsample`](Self::stored_downsample) as CSV, Arrow or Parquet bytes.
	///
	/// # Errors
	///
	/// As [`stored_downsample`](Self::stored_downsample).
	pub async fn stored_downsample_as(&self, aspect: &str, query: &StoredDownsample, format: Format) -> Result<Vec<u8>> {
		let last = format_path("downsample", format, '.', false);
		Ok(self.send(Self::stored_downsample_call(&["api", "v1", "storage", aspect, &last], query)).await?.body)
	}

	// ── maintenance and backup ───────────────────────────────────────────────

	/// `POST /api/v1/storage[/{aspect}]/reconcile`, with any of `threshold`, `hot_cold`,
	/// `overlaps`, `split_min_bytes`.
	///
	/// # Errors
	///
	/// The server's refusal, or a transport failure.
	pub async fn reconcile(&self, aspect: Option<&str>, params: &[(&'static str, String)]) -> Result<serde_json::Value> {
		let path: Vec<&str> = aspect.map_or_else(|| vec!["api", "v1", "storage", "reconcile"], |aspect| vec!["api", "v1", "storage", aspect, "reconcile"]);
		let mut call = Call::post(&path);
		call.query = params.to_vec();
		self.send(call).await?.json()
	}

	/// `POST /api/v1/storage/{aspect}/squash`.
	///
	/// # Errors
	///
	/// The server's refusal, or a transport failure.
	pub async fn squash(&self, aspect: &str, max_segments: Option<u32>) -> Result<serde_json::Value> {
		self.send(Call::post(&["api", "v1", "storage", aspect, "squash"]).param("max_segments", max_segments)).await?.json()
	}

	/// `POST /api/v1/storage/{aspect}/compact` to segments of about `target_rows`.
	///
	/// # Errors
	///
	/// The server's refusal, or a transport failure.
	pub async fn compact(&self, aspect: &str, target_rows: u64) -> Result<serde_json::Value> {
		self.send(Call::post(&["api", "v1", "storage", aspect, "compact"]).param("target_rows", Some(target_rows))).await?.json()
	}

	/// `POST /api/v1/storage/fsck`: run the recovery pass on demand.
	///
	/// # Errors
	///
	/// The server's refusal, or a transport failure.
	pub async fn fsck(&self) -> Result<serde_json::Value> {
		self.send(Call::post(&["api", "v1", "storage", "fsck"])).await?.json()
	}

	/// `GET /api/v1/storage/quarantine`: the quarantined segments, optionally for one aspect.
	///
	/// # Errors
	///
	/// The server's refusal, or a transport failure.
	pub async fn quarantine(&self, aspect: Option<&str>) -> Result<serde_json::Value> {
		self.send(Call::get(&["api", "v1", "storage", "quarantine"]).param("aspect", aspect)).await?.json()
	}

	/// `POST /api/v1/storage/backup`: an online control-plane snapshot (`admin`).
	///
	/// # Errors
	///
	/// The server's refusal, or a transport failure.
	pub async fn backup(&self, label: Option<&str>, verify: Option<&str>) -> Result<serde_json::Value> {
		self.send(Call::post(&["api", "v1", "storage", "backup"]).param("label", label).param("verify", verify)).await?.json()
	}

	/// `POST /api/v1/storage/restore/drill`: rehearse restoring `label` without touching the
	/// live store.
	///
	/// # Errors
	///
	/// The server's refusal, or a transport failure.
	pub async fn restore_drill(&self, label: &str) -> Result<serde_json::Value> {
		self.send(Call::post(&["api", "v1", "storage", "restore", "drill"]).param("label", Some(label))).await?.json()
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn the_base_url_is_an_origin_without_secrets() {
		assert!(Client::new("http://127.0.0.1:8080").is_ok());
		assert!(Client::new("http://127.0.0.1:8080/prefix/").is_ok());
		for bad in ["127.0.0.1:8080", "ftp://host", "http://host/?key=weft_x", "http://user:pass@host"] {
			assert_eq!(Client::new(bad).unwrap_err().kind(), ErrorKind::InvalidInput, "{bad}");
		}
		let debug = format!("{:?}", Client::builder("http://h").api_key("weft_secret").build().unwrap());
		assert!(!debug.contains("weft_secret"), "{debug}");
	}

	#[cfg(not(feature = "rustls"))]
	#[test]
	fn https_without_tls_says_which_feature() {
		let error = Client::new("https://db.example").unwrap_err();
		assert!(error.message().contains("rustls"), "{error}");
	}

	#[test]
	fn an_aspect_is_one_path_segment() {
		let client = Client::new("http://h:1/base").unwrap();
		let url = client.url(&["api", "v1", "storage", "a/../b c", "points"], &[("start", "1".into()), ("lo", "-1.5".into())]);
		assert_eq!(url.as_str(), "http://h:1/base/api/v1/storage/a%2F..%2Fb%20c/points?start=1&lo=-1.5");
		assert_eq!(Client::new("http://h:1").unwrap().url(&["health"], &[]).as_str(), "http://h:1/health");
	}

	#[test]
	fn format_suffixes_follow_each_route_family() {
		assert_eq!(Client::columnar(Format::Arrow, "range").unwrap(), "range");
		assert_eq!(Client::columnar(Format::Parquet, "value-range").unwrap(), "value-range.parquet");
		assert!(Client::columnar(Format::Json, "range").is_err());
		assert_eq!(format_path("downsample", Format::Json, '.', false), "downsample");
		assert_eq!(format_path("downsample", Format::Csv, '.', false), "downsample.csv");
		assert!(spline_param(Some(Spline::Polynomial { degree: 2 })).is_err());
		assert_eq!(spline_param(Some(Spline::Linear)).unwrap(), Some("linear"));
	}
}
