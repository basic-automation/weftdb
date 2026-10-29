//! The sealed route registry (1.0 plan **SEC-3**, resolutions **C4** and **C10**).
//!
//! Before this module the router was one long `.route(...)` chain in `lib.rs`, and
//! everything that needs to know the route table — an authorisation policy, the
//! per-route admission classes ROB-10 wanted, the OpenAPI document FRE-9 will
//! publish — would have had to keep its own copy of that table. Four copies is
//! what C4 calls out: SEC-3's sealed builder, FRE-9's `OpenApiRouter`, FRE-24's
//! roles and ROB-10's classes, each able to drift from the others and from the
//! router actually served.
//!
//! This module is the one registry. A route exists only by being registered
//! here, with its [`RouteSpec`], through one of the per-area registration
//! functions at the bottom of the file (`register_probe_routes`,
//! `register_storage_read_routes`, …). Later slices add routes **in those area
//! functions** (or a new one for a new area), never by editing a chain in
//! `lib.rs` — that is the L-ROUTER lane rule from the plan §2, and it is what
//! makes the rewrite of `lib.rs` a one-time event.
//!
//! One registry, four products:
//!
//! - the [`axum::Router`] itself ([`SealedRoutes::router`]);
//! - the **policy table** ([`RouteTable::policy_table`]): method, path and
//!   [`Scope`] per route — the table SEC-6's Bearer gate will enforce;
//! - the **class table** ([`RouteTable::class_table`]): [`AdmissionClass`] and
//!   [`Tier`] per operation — ROB-10's per-route classes, folded in here;
//! - the **OpenAPI document** ([`RouteTable::openapi_document`]): one operation
//!   per route, keyed by its frozen `operation_id`, carrying its spec as
//!   `x-weft-*` extensions. FRE-9a later feeds utoipa-axum's `routes!()` output
//!   *into* [`Routes::route`]; the document's shape does not change then, only
//!   where the handler metadata comes from.
//!
//! ## The gate is fail-closed
//!
//! [`SealedRoutes::router`] installs the gate as a layer. The gate looks the
//! request's matched path up in the sealed table: a matched route whose
//! (method, path) pair has no entry is refused — `405 method_not_allowed` with
//! an `Allow` header when the path is registered under another method (the
//! FRE-6a 405 fallback, absorbed here), and anything that matches no route at
//! all falls through to the fallback, which answers `404 route_not_found` (or
//! the same 405 when the concrete path matches a registered template under
//! another method). There is no path from a request to a handler that does not
//! pass a table entry, because the router is built from the table and nothing
//! else. The matched route's [`RouteSpec`] is left in the request extensions
//! for the layers that follow (SEC-6's auth check, SEC-9's audit).
//!
//! ## The frozen scope table
//!
//! [`Scope`] is the whole vocabulary a route may ask for, and it is **frozen**
//! for 1.0: eight scopes, in the order [`Scope::ALL`] lists them, with the
//! `as_str` names below. Two names are the plan's own — `maintain` (§5, D-S12:
//! fsck and quarantine routes register "with scope `maintain`") and `metrics`
//! (§11 decision 17: "`/metrics` needs the metrics scope when auth is on"). The
//! other six name the route areas the server already has, one scope per area
//! module group: probes, storage reads, the stateless compute endpoints,
//! ingest, aspect declaration and backup. Adding a ninth scope after 1.0 is a
//! policy decision for a later release, not something a route-adding slice may
//! do in passing; the `scope_table_is_frozen` test pins the table so the change
//! is deliberate and reviewable when it comes.
//!
//! [`Role`] is the other half of the §3 row: FRE's four roles are replaced by
//! SEC's eight scopes and **six roles**. A role is only a frozen grant of
//! scopes ([`Role::scopes`]); which principal holds which role is SEC-6's
//! business (API keys), not this module's. The grants are cumulative — each
//! role holds everything the one before it holds — so "can this key call this
//! route" is one membership test against the route's scope.

use std::{collections::BTreeMap, sync::Arc};

use axum::{
	extract::{Extension, MatchedPath}, http::{header, HeaderValue, Method, Uri}, middleware::{self, Next}, response::{IntoResponse, Response}, routing::{get, post, MethodRouter}, Router
};
use serde_json::{json, Map, Value};

use crate::{
	backup_routes, declare, downsample, error::{ApiError, Code}, ingest, interpolate, maintain, metrics, settings::Tier, state::AppState, storage, HealthResponse, ReadyResponse
};

/// What a request to a route must hold, from the frozen table above. See the
/// module docs for why there are exactly eight and why the table is frozen.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Scope {
	/// Liveness and readiness probes (`/health`, `/ready`): the only scope an
	/// unauthenticated orchestrator needs.
	Probe,
	/// Operational telemetry (`/metrics`, the current-profile debug route).
	/// Decision 17 names this scope for `/metrics` once auth is on.
	Metrics,
	/// Stored-data reads: ranges, points, catalog, stats, schema.
	Read,
	/// The stateless compute endpoints (interpolate, downsample) that read
	/// only what the request body carries.
	Compute,
	/// Writing points into the store (every ingest format).
	Ingest,
	/// Declaring an aspect's schema — the catalog write the other writes
	/// depend on, held apart from [`Scope::Ingest`] so a pure ingest key
	/// cannot reshape the catalog.
	Declare,
	/// Store maintenance: reconcile, squash, compact — and, when D-S12 lands
	/// them through this registry, fsck and the quarantine routes.
	Maintain,
	/// Backup and restore-drill: whole-store operations, held apart from
	/// [`Scope::Maintain`] because a backup reads everything.
	Backup,
}

impl Scope {
	/// The frozen scope table, in its frozen order. A test pins this list;
	/// changing it is a 1.0-policy change, not a route-adding act.
	pub const ALL: [Self; 8] = [Self::Probe, Self::Metrics, Self::Read, Self::Compute, Self::Ingest, Self::Declare, Self::Maintain, Self::Backup];

	/// The scope's name in the policy table, the OpenAPI extensions and (from
	/// SEC-6) the keys file. Frozen with the table.
	#[must_use]
	pub const fn as_str(self) -> &'static str {
		match self {
			Self::Probe => "probe",
			Self::Metrics => "metrics",
			Self::Read => "read",
			Self::Compute => "compute",
			Self::Ingest => "ingest",
			Self::Declare => "declare",
			Self::Maintain => "maintain",
			Self::Backup => "backup",
		}
	}
}

/// A named grant of scopes — SEC's six roles, replacing FRE's four. Only the
/// grant is defined here; SEC-6 decides which key holds which role.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Role {
	/// Probes only: an orchestrator or load balancer that may learn the
	/// process is up and nothing else.
	Anonymous,
	/// Reads stored data and runs stateless compute.
	Reader,
	/// A [`Role::Reader`] that may also ingest points.
	Writer,
	/// A [`Role::Writer`] that may also declare aspect schemas.
	Declarer,
	/// A [`Role::Declarer`] that may also run store maintenance and read
	/// the metrics surface.
	Maintainer,
	/// Everything, including backup: the operator's own key.
	Admin,
}

impl Role {
	/// The six roles, in grant order (each holds the previous one's scopes).
	pub const ALL: [Self; 6] = [Self::Anonymous, Self::Reader, Self::Writer, Self::Declarer, Self::Maintainer, Self::Admin];

	/// The role's frozen name.
	#[must_use]
	pub const fn as_str(self) -> &'static str {
		match self {
			Self::Anonymous => "anonymous",
			Self::Reader => "reader",
			Self::Writer => "writer",
			Self::Declarer => "declarer",
			Self::Maintainer => "maintainer",
			Self::Admin => "admin",
		}
	}

	/// The scopes this role grants. Cumulative by construction: each list is
	/// the previous role's list plus its own addition, written out in full so
	/// the frozen table reads as a table.
	#[must_use]
	pub const fn scopes(self) -> &'static [Scope] {
		match self {
			Self::Anonymous => &[Scope::Probe],
			Self::Reader => &[Scope::Probe, Scope::Read, Scope::Compute],
			Self::Writer => &[Scope::Probe, Scope::Read, Scope::Compute, Scope::Ingest],
			Self::Declarer => &[Scope::Probe, Scope::Read, Scope::Compute, Scope::Ingest, Scope::Declare],
			Self::Maintainer => &[Scope::Probe, Scope::Metrics, Scope::Read, Scope::Compute, Scope::Ingest, Scope::Declare, Scope::Maintain],
			Self::Admin => &Scope::ALL,
		}
	}

	/// Whether this role may call a route asking for `scope`.
	#[must_use]
	pub fn allows(self, scope: Scope) -> bool {
		self.scopes().contains(&scope)
	}
}

/// Whether a route's calls are written to the audit log (SEC-9's fsynced
/// JSONL file), and how. The registry records the requirement; SEC-9 owns
/// the writer, and a route whose spec says [`Audit::AttemptOutcome`] will
/// fail closed on `audit_unavailable` once that writer exists.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Audit {
	/// No audit record: probes, reads and stateless compute.
	None,
	/// An attempt record and an outcome record per call (plan §11,
	/// decision 8: "per admin or mutating operation"). Every mutating
	/// route — ingest, declare, maintenance, backup — carries this.
	AttemptOutcome,
}

impl Audit {
	/// The audit requirement's name in the OpenAPI extensions.
	#[must_use]
	pub const fn as_str(self) -> &'static str {
		match self {
			Self::None => "none",
			Self::AttemptOutcome => "attempt_outcome",
		}
	}
}

/// How a route competes for admission when the server is loaded — ROB-10's
/// per-route classes, folded into this registry by §3. D-S14 puts ingest
/// admission *inside* the gate (C10); this is the class it will consult.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum AdmissionClass {
	/// Probes: never shed. A shed probe reads as a dead process and starts
	/// the restart machinery, which is the most expensive answer available.
	Probe,
	/// Reads and stateless compute: bounded by the read limits (decision
	/// 12) and shed first under load.
	Query,
	/// Point writes and declarations: admitted against the body cap
	/// (`WEFT_MAX_INGEST_BYTES`, C11) rather than the read limits.
	Write,
	/// Maintenance and backup: long, exclusive-flavoured work; a second
	/// admission while one runs is the `409 busy` the handlers already
	/// answer.
	Maintenance,
}

impl AdmissionClass {
	/// The class's name in the class table and the OpenAPI extensions.
	#[must_use]
	pub const fn as_str(self) -> &'static str {
		match self {
			Self::Probe => "probe",
			Self::Query => "query",
			Self::Write => "write",
			Self::Maintenance => "maintenance",
		}
	}
}

/// One route's contract, exactly the §3 field set: every route registered in
/// this module carries one, and the four products (router, policy table,
/// class table, OpenAPI) are all read off these values.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RouteSpec {
	/// The [`Scope`] a caller must hold for this route (SEC-6 enforces).
	pub scope: Scope,
	/// Whether calls are audited, and how ([`Audit`]).
	pub audit: Audit,
	/// Whether a `5xx` on this route may carry its real cause to the caller.
	/// X-ERR redacts `5xx` detail unconditionally until this registry
	/// exists; the routes marked here (maintenance, backup, the operator
	/// profile) are the ones whose callers are operators by definition.
	pub operator_detail: bool,
	/// How much the route itself is promised — the same [`Tier`] vocabulary
	/// as the configuration registry's `VarSpec`. Every route the server
	/// ships today is part of the frozen 1.0 surface, so every spec built
	/// by [`RouteSpec::stable`] carries [`Tier::Stable`].
	pub tier: Tier,
	/// The route's [`AdmissionClass`].
	pub admission_class: AdmissionClass,
	/// The route's stable operation name — its OpenAPI `operationId`, its
	/// audit and metrics label, and its key in the class table. Unique
	/// across the registry; registration panics on a duplicate.
	pub operation_id: &'static str,
}

impl RouteSpec {
	/// A spec for a route on the frozen 1.0 surface ([`Tier::Stable`]).
	#[must_use]
	pub const fn stable(scope: Scope, audit: Audit, operator_detail: bool, admission_class: AdmissionClass, operation_id: &'static str) -> Self {
		Self { scope, audit, operator_detail, tier: Tier::Stable, admission_class, operation_id }
	}
}

/// One registered route: its method, its path template and its spec. The
/// registry's tables are lists of these. (Not `Copy`: `Method` is not.)
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RouteEntry {
	/// The HTTP method this entry serves. A path serving two methods has
	/// two entries (and two operation ids).
	pub method: Method,
	/// The axum path template, e.g. `/api/v1/storage/{aspect}/range`.
	pub path: &'static str,
	/// The route's contract.
	pub spec: RouteSpec,
}

/// The sealed registry's read side: the tables every consumer shares. Built
/// once by [`Routes::seal`]; there is no way to add to it afterwards, which
/// is what "sealed" means here — the router and the tables cannot drift
/// because both are this one value.
#[derive(Debug)]
pub struct RouteTable {
	entries: Vec<RouteEntry>,
}

impl RouteTable {
	/// Every registered route, in registration order. This list *is* the
	/// policy table and the class table; the two accessors below project
	/// the fields each consumer reads.
	#[must_use]
	pub fn entries(&self) -> &[RouteEntry] {
		&self.entries
	}

	/// The entry for one (method, matched path template) pair, if the
	/// registry knows it. The gate's whole decision is this lookup.
	#[must_use]
	pub fn lookup(&self, method: &Method, matched_path: &str) -> Option<&RouteEntry> {
		self.entries.iter().find(|entry| &entry.method == method && entry.path == matched_path)
	}

	/// The entry whose spec carries `operation_id`, if any.
	#[must_use]
	pub fn by_operation(&self, operation_id: &str) -> Option<&RouteEntry> {
		self.entries.iter().find(|entry| entry.spec.operation_id == operation_id)
	}

	/// The methods registered for a path template — the `Allow` set for a
	/// 405 on that path.
	#[must_use]
	pub fn allowed_methods(&self, path: &str) -> Vec<Method> {
		self.entries.iter().filter(|entry| entry.path == path).map(|entry| entry.method.clone()).collect()
	}

	/// The methods registered for any template a concrete request path
	/// matches — the fallback's `Allow` set, where no route matched and so
	/// no template is known.
	#[must_use]
	pub fn allowed_methods_for_concrete(&self, concrete_path: &str) -> Vec<Method> {
		self.entries.iter().filter(|entry| template_matches(entry.path, concrete_path)).map(|entry| entry.method.clone()).collect()
	}

	/// The policy table SEC-6 enforces: (method, path, operation, scope)
	/// per route, projected from the one registry.
	#[must_use]
	pub fn policy_table(&self) -> Vec<PolicyEntry> {
		self.entries.iter().map(|entry| PolicyEntry { method: entry.method.clone(), path: entry.path, operation_id: entry.spec.operation_id, scope: entry.spec.scope }).collect()
	}

	/// The class table: (operation, admission class, tier) per route —
	/// ROB-10's per-route classes, projected from the one registry.
	#[must_use]
	pub fn class_table(&self) -> Vec<ClassEntry> {
		self.entries.iter().map(|entry| ClassEntry { operation_id: entry.spec.operation_id, admission_class: entry.spec.admission_class, tier: entry.spec.tier }).collect()
	}

	/// The OpenAPI document for the whole surface, generated from the one
	/// registry: one path item per template, one operation per route,
	/// keyed by the frozen `operation_id` and carrying the rest of the
	/// spec as `x-weft-*` extensions, so a consumer that only reads the
	/// document sees the same policy the gate enforces.
	#[must_use]
	pub fn openapi_document(&self) -> Value {
		let mut paths: BTreeMap<&'static str, Map<String, Value>> = BTreeMap::new();
		for entry in &self.entries {
			let spec = &entry.spec;
			let operation = json!({
				"operationId": spec.operation_id,
				"x-weft-scope": spec.scope.as_str(),
				"x-weft-audit": spec.audit.as_str(),
				"x-weft-operator-detail": spec.operator_detail,
				"x-weft-tier": spec.tier.label(),
				"x-weft-admission-class": spec.admission_class.as_str(),
				"responses": { "200": { "description": "OK" } },
			});
			paths.entry(entry.path).or_default().insert(method_key(&entry.method).to_string(), operation);
		}
		json!({
			"openapi": "3.1.0",
			"info": { "title": crate::SERVICE, "version": crate::VERSION },
			"paths": paths,
		})
	}
}

/// One row of the policy table: what a route demands of its caller.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PolicyEntry {
	/// The HTTP method.
	pub method: Method,
	/// The path template.
	pub path: &'static str,
	/// The route's operation id.
	pub operation_id: &'static str,
	/// The scope the caller must hold.
	pub scope: Scope,
}

/// One row of the class table: how an operation is admitted and how much
/// it is promised.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ClassEntry {
	/// The route's operation id.
	pub operation_id: &'static str,
	/// The route's admission class.
	pub admission_class: AdmissionClass,
	/// The route's tier.
	pub tier: Tier,
}

/// Whether a path template matches a concrete request path, segment by
/// segment: a `{param}` segment matches any single non-empty segment,
/// every other segment must be equal, and the counts must agree.
fn template_matches(template: &str, concrete: &str) -> bool {
	let mut template_parts = template.split('/');
	let mut concrete_parts = concrete.split('/');
	loop {
		match (template_parts.next(), concrete_parts.next()) {
			(None, None) => return true,
			(Some(part), Some(segment)) => {
				if part.starts_with('{') && part.ends_with('}') {
					if segment.is_empty() {
						return false;
					}
				} else if part != segment {
					return false;
				}
			}
			_ => return false,
		}
	}
}

/// The OpenAPI key for a method (`get`, `post`, …).
fn method_key(method: &Method) -> &'static str {
	match *method {
		Method::GET => "get",
		Method::POST => "post",
		Method::PUT => "put",
		Method::PATCH => "patch",
		Method::DELETE => "delete",
		Method::HEAD => "head",
		Method::OPTIONS => "options",
		_ => "other",
	}
}

/// The registry under construction. Routes enter only through
/// [`Routes::route`], called by the per-area registration functions;
/// [`Routes::seal`] freezes the result into a [`SealedRoutes`], after
/// which registration is impossible (the builder is consumed).
#[derive(Default)]
pub struct Routes {
	entries: Vec<RouteEntry>,
	handlers: Vec<(&'static str, MethodRouter<AppState>)>,
}

impl Routes {
	/// An empty registry.
	#[must_use]
	pub fn new() -> Self {
		Self::default()
	}

	/// Register one route: a method, a path template, its spec and its
	/// handler. Later routes are added by calling this from an area
	/// registration function — never by building a router elsewhere.
	///
	/// # Panics
	///
	/// On a duplicate (method, path) pair or a duplicate `operation_id`.
	/// Both are programming errors in a frozen table, caught the first
	/// time the server (or a test) builds its registry — fail-closed at
	/// startup rather than a route silently shadowed at runtime.
	pub fn route(&mut self, method: Method, path: &'static str, spec: RouteSpec, handler: MethodRouter<AppState>) -> &mut Self {
		assert!(!spec.operation_id.is_empty(), "a route's operation_id must not be empty (path {path})");
		assert!(self.entries.iter().all(|entry| entry.spec.operation_id != spec.operation_id), "duplicate operation_id `{}` in the route registry", spec.operation_id);
		assert!(self.entries.iter().all(|entry| !(entry.method == method && entry.path == path)), "duplicate route {method} {path} in the route registry");
		self.entries.push(RouteEntry { method, path, spec });
		if let Some((_, existing)) = self.handlers.iter_mut().find(|(known, _)| *known == path) {
			// A second method on a known path merges into its method
			// router: axum keys the router by path, the registry by
			// (method, path).
			let merged = std::mem::take(existing).merge(handler);
			*existing = merged;
		} else {
			self.handlers.push((path, handler));
		}
		self
	}

	/// Freeze the registry. The returned value builds the router and
	/// serves the tables; this builder no longer exists, so nothing can
	/// register into a registry that is already being served.
	#[must_use]
	pub fn seal(self) -> SealedRoutes {
		let table = Arc::new(RouteTable { entries: self.entries });
		SealedRoutes { table, handlers: self.handlers }
	}
}

/// A frozen registry: the router product plus the shared [`RouteTable`].
pub struct SealedRoutes {
	table: Arc<RouteTable>,
	handlers: Vec<(&'static str, MethodRouter<AppState>)>,
}

impl SealedRoutes {
	/// The shared table (policy, classes, OpenAPI, gate lookups).
	#[must_use]
	pub fn table(&self) -> Arc<RouteTable> {
		Arc::clone(&self.table)
	}

	/// The OpenAPI document for this registry (see
	/// [`RouteTable::openapi_document`]).
	#[must_use]
	pub fn openapi_document(&self) -> Value {
		self.table.openapi_document()
	}

	/// Build the router from the registry — and only from the registry.
	/// The fail-closed gate layer and the registry fallback are part
	/// of the product, so a router built any other way is not this
	/// registry's router.
	#[must_use]
	pub fn router(self) -> Router<AppState> {
		let mut router = Router::new();
		for (path, handler) in self.handlers {
			router = router.route(path, handler);
		}
		router.fallback(route_not_found).layer(middleware::from_fn_with_state(self.table, gate))
	}
}

/// The whole surface's registry, assembled from the per-area registration
/// functions. This is the function [`crate::app_with_state`] builds the
/// served router from, and the one the table tests read.
#[must_use]
pub fn build_registry() -> SealedRoutes {
	let mut routes = Routes::new();
	register_probe_routes(&mut routes);
	register_metrics_routes(&mut routes);
	register_compute_routes(&mut routes);
	register_storage_read_routes(&mut routes);
	register_ingest_routes(&mut routes);
	register_declare_routes(&mut routes);
	register_maintenance_routes(&mut routes);
	register_backup_routes(&mut routes);
	routes.seal()
}

/// The OpenAPI document for the whole surface, from the one registry.
#[must_use]
pub fn openapi_document() -> Value {
	build_registry().openapi_document()
}

/// The fail-closed gate. A request whose matched path is registered under
/// its method passes, carrying its [`RouteSpec`] in its extensions; a
/// matched path registered only under other methods is refused here with
/// the 405 the code table names; a request with no matched path passes
/// through to the fallback, which refuses it — the gate's table rides
/// along in the extensions so the fallback decides from the same table.
async fn gate(axum::extract::State(table): axum::extract::State<Arc<RouteTable>>, mut request: axum::extract::Request, next: Next) -> Response {
	let matched = request.extensions().get::<MatchedPath>().map(|matched| matched.as_str().to_string());
	request.extensions_mut().insert(Arc::clone(&table));
	if let Some(matched) = matched {
		if let Some(entry) = table.lookup(request.method(), &matched) {
			let spec = entry.spec;
			request.extensions_mut().insert(spec);
			return next.run(request).await;
		}
		let allowed = table.allowed_methods(&matched);
		if !allowed.is_empty() {
			return method_not_allowed(request.method(), &matched, &allowed);
		}
	}
	next.run(request).await
}

/// A `405 method_not_allowed` in the error envelope, with the `Allow`
/// header the §4 code table pairs with the code.
fn method_not_allowed(method: &Method, path: &str, allowed: &[Method]) -> Response {
	let allow = allowed.iter().map(Method::as_str).collect::<Vec<_>>().join(", ");
	let mut response = ApiError::new(Code::MethodNotAllowed, format!("{method} is not served on `{path}`; this route serves {allow}")).into_response();
	if let Ok(value) = HeaderValue::from_str(&allow) {
		response.headers_mut().insert(header::ALLOW, value);
	}
	response
}

/// The fallback for a path no route matches, decided from the sealed
/// table the gate left in the extensions.
///
/// Before X-ERR an unmatched path got axum's bare `404` with an empty
/// body, so a client that mistyped a route could not tell that from a
/// route that exists and refused it. The code and the sentence together
/// say "there is no such endpoint" — and when the concrete path matches
/// a registered template under another method, the answer is the 405
/// instead, with `Allow`: `GET /api/v1/storage/price/points` exists,
/// `DELETE` there does not, and the status is where that shows.
async fn route_not_found(Extension(table): Extension<Arc<RouteTable>>, method: Method, uri: Uri) -> Response {
	let allowed = table.allowed_methods_for_concrete(uri.path());
	if !allowed.is_empty() {
		return method_not_allowed(&method, uri.path(), &allowed);
	}
	ApiError::new(Code::RouteNotFound, format!("no route matches {method} `{}`; `GET /api/v1/storage/aspects` lists the aspects, and the API guide at `docs/INTEGRATION.md` lists the endpoints", uri.path())).into_response()
}

// ── per-area registration ────────────────────────────────────────────────────
//
// One function per route area. A later slice adding a route edits its
// area's function here (or adds a function for a new area and one call in
// `build_registry`); `lib.rs` never changes again for a route.

/// Probes: liveness and readiness.
fn register_probe_routes(routes: &mut Routes) {
	routes.route(Method::GET, "/health", RouteSpec::stable(Scope::Probe, Audit::None, false, AdmissionClass::Probe, "get_health"), get(health));
	routes.route(Method::GET, "/ready", RouteSpec::stable(Scope::Probe, Audit::None, false, AdmissionClass::Probe, "get_ready"), get(ready));
}

/// Liveness probe: the process is up and can serve a request.
async fn health() -> axum::Json<HealthResponse> {
	axum::Json(HealthResponse::default())
}

/// Readiness probe: the service is ready to accept traffic. Reports
/// whether a segment store is configured so an operator can confirm the
/// storage endpoints are live, and whether that store is write-poisoned
/// and needs a restart. With a store, it also carries the store's identity,
/// durability class, previous clean shutdown and the last recovery pass's
/// counts (D-S12) — and answers `503` when that pass reports
/// `incompatible_store`: a newer-format frame means this build must not serve
/// this store. Without a store the stateless API is ready, as before.
async fn ready(axum::extract::State(state): axum::extract::State<AppState>) -> (axum::http::StatusCode, axum::Json<ReadyResponse>) {
	let Some(store) = state.store() else {
		return (axum::http::StatusCode::OK, axum::Json(ReadyResponse::default()));
	};
	let poison = store.poisoned();
	let report = store.recovery_report();
	let ready = !report.incompatible_store;
	let status = if ready { axum::http::StatusCode::OK } else { axum::http::StatusCode::SERVICE_UNAVAILABLE };
	let body = ReadyResponse { ready, segment_store: true, store_uuid: Some(store.store_uuid().to_string()), durability_class: Some(weftdb::SegmentStore::durability_class()), previous_clean_shutdown: Some(store.previous_clean_shutdown()), quarantined: report.quarantined, suspect: report.suspected, incompatible_store: report.incompatible_store, ..ReadyResponse::default() }.with_poison(poison);
	(status, axum::Json(body))
}

/// Operational telemetry: the Prometheus surface and the operator's
/// current-profile view. Decision 17: `/metrics` needs the metrics scope
/// once auth is on.
fn register_metrics_routes(routes: &mut Routes) {
	routes.route(Method::GET, "/metrics", RouteSpec::stable(Scope::Metrics, Audit::None, false, AdmissionClass::Query, "get_metrics"), get(metrics::metrics));
	routes.route(Method::GET, "/debug/profile/current", RouteSpec::stable(Scope::Metrics, Audit::None, true, AdmissionClass::Query, "get_debug_profile_current"), get(metrics::profile_current));
}

/// Stateless compute: interpolation and downsampling over what the
/// request body carries. No store is touched, so no audit record.
fn register_compute_routes(routes: &mut Routes) {
	routes.route(Method::POST, "/api/v1/interpolate", RouteSpec::stable(Scope::Compute, Audit::None, false, AdmissionClass::Query, "interpolate"), post(interpolate::interpolate));
	routes.route(Method::POST, "/api/v1/interpolate/csv", RouteSpec::stable(Scope::Compute, Audit::None, false, AdmissionClass::Query, "interpolate_csv"), post(interpolate::interpolate_csv));
	routes.route(Method::POST, "/api/v1/interpolate/arrow", RouteSpec::stable(Scope::Compute, Audit::None, false, AdmissionClass::Query, "interpolate_arrow"), post(interpolate::interpolate_arrow));
	routes.route(Method::POST, "/api/v1/interpolate/parquet", RouteSpec::stable(Scope::Compute, Audit::None, false, AdmissionClass::Query, "interpolate_parquet"), post(interpolate::interpolate_parquet));
	routes.route(Method::POST, "/api/v1/interpolate/ilp", RouteSpec::stable(Scope::Compute, Audit::None, false, AdmissionClass::Query, "interpolate_ilp"), post(interpolate::interpolate_ilp));
	routes.route(Method::POST, "/api/v1/interpolate/ilp/csv", RouteSpec::stable(Scope::Compute, Audit::None, false, AdmissionClass::Query, "interpolate_ilp_csv"), post(interpolate::interpolate_ilp_csv));
	routes.route(Method::POST, "/api/v1/interpolate/ilp/arrow", RouteSpec::stable(Scope::Compute, Audit::None, false, AdmissionClass::Query, "interpolate_ilp_arrow"), post(interpolate::interpolate_ilp_arrow));
	routes.route(Method::POST, "/api/v1/interpolate/ilp/parquet", RouteSpec::stable(Scope::Compute, Audit::None, false, AdmissionClass::Query, "interpolate_ilp_parquet"), post(interpolate::interpolate_ilp_parquet));
	routes.route(Method::POST, "/api/v1/interpolate/point", RouteSpec::stable(Scope::Compute, Audit::None, false, AdmissionClass::Query, "interpolate_point"), post(interpolate::interpolate_point));
	routes.route(Method::POST, "/api/v1/downsample", RouteSpec::stable(Scope::Compute, Audit::None, false, AdmissionClass::Query, "downsample"), post(downsample::downsample));
	routes.route(Method::POST, "/api/v1/downsample/csv", RouteSpec::stable(Scope::Compute, Audit::None, false, AdmissionClass::Query, "downsample_csv"), post(downsample::downsample_csv));
	routes.route(Method::POST, "/api/v1/downsample/arrow", RouteSpec::stable(Scope::Compute, Audit::None, false, AdmissionClass::Query, "downsample_arrow"), post(downsample::downsample_arrow));
	routes.route(Method::POST, "/api/v1/downsample/parquet", RouteSpec::stable(Scope::Compute, Audit::None, false, AdmissionClass::Query, "downsample_parquet"), post(downsample::downsample_parquet));
	routes.route(Method::POST, "/api/v1/downsample/ilp", RouteSpec::stable(Scope::Compute, Audit::None, false, AdmissionClass::Query, "downsample_ilp"), post(downsample::downsample_ilp));
	routes.route(Method::POST, "/api/v1/downsample/ilp/csv", RouteSpec::stable(Scope::Compute, Audit::None, false, AdmissionClass::Query, "downsample_ilp_csv"), post(downsample::downsample_ilp_csv));
	routes.route(Method::POST, "/api/v1/downsample/ilp/arrow", RouteSpec::stable(Scope::Compute, Audit::None, false, AdmissionClass::Query, "downsample_ilp_arrow"), post(downsample::downsample_ilp_arrow));
	routes.route(Method::POST, "/api/v1/downsample/ilp/parquet", RouteSpec::stable(Scope::Compute, Audit::None, false, AdmissionClass::Query, "downsample_ilp_parquet"), post(downsample::downsample_ilp_parquet));
}

/// Stored-data reads. The per-aspect downsample paths serve both verbs
/// with one handler (the POST carries the parameters in its body); both
/// are reads of the store, so both carry the read scope.
fn register_storage_read_routes(routes: &mut Routes) {
	routes.route(Method::GET, "/api/v1/storage/aspects", RouteSpec::stable(Scope::Read, Audit::None, false, AdmissionClass::Query, "storage_aspects_list"), get(storage::storage_aspects));
	routes.route(Method::GET, "/api/v1/storage/catalog", RouteSpec::stable(Scope::Read, Audit::None, false, AdmissionClass::Query, "storage_catalog"), get(storage::storage_catalog));
	routes.route(Method::GET, "/api/v1/storage/stats", RouteSpec::stable(Scope::Read, Audit::None, false, AdmissionClass::Query, "storage_stats"), get(storage::storage_stats));
	routes.route(Method::GET, "/api/v1/storage/{aspect}/range", RouteSpec::stable(Scope::Read, Audit::None, false, AdmissionClass::Query, "storage_time_range"), get(storage::storage_time_range));
	routes.route(Method::GET, "/api/v1/storage/{aspect}/range.parquet", RouteSpec::stable(Scope::Read, Audit::None, false, AdmissionClass::Query, "storage_time_range_parquet"), get(storage::storage_time_range_parquet));
	routes.route(Method::GET, "/api/v1/storage/{aspect}/range.csv", RouteSpec::stable(Scope::Read, Audit::None, false, AdmissionClass::Query, "storage_time_range_csv"), get(storage::storage_time_range_csv));
	routes.route(Method::GET, "/api/v1/storage/{aspect}/points", RouteSpec::stable(Scope::Read, Audit::None, false, AdmissionClass::Query, "storage_points_list"), get(storage::storage_time_range_json));
	routes.route(Method::GET, "/api/v1/storage/{aspect}/downsample", RouteSpec::stable(Scope::Read, Audit::None, false, AdmissionClass::Query, "storage_downsample_get"), get(storage::storage_downsample));
	routes.route(Method::POST, "/api/v1/storage/{aspect}/downsample", RouteSpec::stable(Scope::Read, Audit::None, false, AdmissionClass::Query, "storage_downsample_post"), post(storage::storage_downsample));
	routes.route(Method::GET, "/api/v1/storage/{aspect}/downsample.csv", RouteSpec::stable(Scope::Read, Audit::None, false, AdmissionClass::Query, "storage_downsample_csv_get"), get(storage::storage_downsample_csv));
	routes.route(Method::POST, "/api/v1/storage/{aspect}/downsample.csv", RouteSpec::stable(Scope::Read, Audit::None, false, AdmissionClass::Query, "storage_downsample_csv_post"), post(storage::storage_downsample_csv));
	routes.route(Method::GET, "/api/v1/storage/{aspect}/downsample.arrow", RouteSpec::stable(Scope::Read, Audit::None, false, AdmissionClass::Query, "storage_downsample_arrow_get"), get(storage::storage_downsample_arrow));
	routes.route(Method::POST, "/api/v1/storage/{aspect}/downsample.arrow", RouteSpec::stable(Scope::Read, Audit::None, false, AdmissionClass::Query, "storage_downsample_arrow_post"), post(storage::storage_downsample_arrow));
	routes.route(Method::GET, "/api/v1/storage/{aspect}/downsample.parquet", RouteSpec::stable(Scope::Read, Audit::None, false, AdmissionClass::Query, "storage_downsample_parquet_get"), get(storage::storage_downsample_parquet));
	routes.route(Method::POST, "/api/v1/storage/{aspect}/downsample.parquet", RouteSpec::stable(Scope::Read, Audit::None, false, AdmissionClass::Query, "storage_downsample_parquet_post"), post(storage::storage_downsample_parquet));
	routes.route(Method::GET, "/api/v1/storage/{aspect}/at", RouteSpec::stable(Scope::Read, Audit::None, false, AdmissionClass::Query, "storage_point_at"), get(storage::storage_point));
	routes.route(Method::GET, "/api/v1/storage/{aspect}/at-multi", RouteSpec::stable(Scope::Read, Audit::None, false, AdmissionClass::Query, "storage_points_at_multi"), get(storage::storage_points));
	routes.route(Method::GET, "/api/v1/storage/{aspect}/value-range", RouteSpec::stable(Scope::Read, Audit::None, false, AdmissionClass::Query, "storage_value_range"), get(storage::storage_value_range));
	routes.route(Method::GET, "/api/v1/storage/{aspect}/value-range.parquet", RouteSpec::stable(Scope::Read, Audit::None, false, AdmissionClass::Query, "storage_value_range_parquet"), get(storage::storage_value_range_parquet));
	routes.route(Method::GET, "/api/v1/storage/{aspect}/value-range.csv", RouteSpec::stable(Scope::Read, Audit::None, false, AdmissionClass::Query, "storage_value_range_csv"), get(storage::storage_value_range_csv));
	routes.route(Method::GET, "/api/v1/storage/{aspect}/value-points", RouteSpec::stable(Scope::Read, Audit::None, false, AdmissionClass::Query, "storage_value_points"), get(storage::storage_value_range_json));
	routes.route(Method::GET, "/api/v1/storage/{aspect}/stats", RouteSpec::stable(Scope::Read, Audit::None, false, AdmissionClass::Query, "storage_aspect_stats"), get(storage::storage_aspect_stats));
	routes.route(Method::GET, "/api/v1/storage/{aspect}/schema", RouteSpec::stable(Scope::Read, Audit::None, false, AdmissionClass::Query, "storage_aspect_schema"), get(storage::storage_aspect_schema));
}

/// Point writes, one route per wire format. Mutating, so audited;
/// admission is the write class D-S14 will put inside this gate (C10).
fn register_ingest_routes(routes: &mut Routes) {
	routes.route(Method::POST, "/api/v1/storage/{aspect}/points", RouteSpec::stable(Scope::Ingest, Audit::AttemptOutcome, false, AdmissionClass::Write, "storage_ingest_points"), post(ingest::ingest_points));
	routes.route(Method::POST, "/api/v1/storage/{aspect}/ilp", RouteSpec::stable(Scope::Ingest, Audit::AttemptOutcome, false, AdmissionClass::Write, "storage_ingest_ilp"), post(ingest::ingest_ilp));
	routes.route(Method::POST, "/api/v1/storage/{aspect}/csv", RouteSpec::stable(Scope::Ingest, Audit::AttemptOutcome, false, AdmissionClass::Write, "storage_ingest_csv"), post(ingest::ingest_csv));
	routes.route(Method::POST, "/api/v1/storage/{aspect}/parquet", RouteSpec::stable(Scope::Ingest, Audit::AttemptOutcome, false, AdmissionClass::Write, "storage_ingest_parquet"), post(ingest::ingest_parquet));
}

/// Catalog writes: declaring an aspect's schema. Its own scope, apart
/// from ingest, per the frozen table's rationale above.
fn register_declare_routes(routes: &mut Routes) {
	routes.route(Method::POST, "/api/v1/storage/aspects", RouteSpec::stable(Scope::Declare, Audit::AttemptOutcome, false, AdmissionClass::Write, "storage_aspect_declare"), post(declare::declare_aspect));
}

/// Store maintenance. Operator routes: audited, and a `5xx` here may
/// carry its cause (`operator_detail`), because the only callers are
/// operators. D-S12's fsck and quarantine routes register here with
/// this same scope when they land.
fn register_maintenance_routes(routes: &mut Routes) {
	routes.route(Method::POST, "/api/v1/storage/reconcile", RouteSpec::stable(Scope::Maintain, Audit::AttemptOutcome, true, AdmissionClass::Maintenance, "storage_reconcile_store"), post(maintain::reconcile_store));
	routes.route(Method::POST, "/api/v1/storage/{aspect}/reconcile", RouteSpec::stable(Scope::Maintain, Audit::AttemptOutcome, true, AdmissionClass::Maintenance, "storage_reconcile_aspect"), post(maintain::reconcile_aspect));
	routes.route(Method::POST, "/api/v1/storage/{aspect}/squash", RouteSpec::stable(Scope::Maintain, Audit::AttemptOutcome, true, AdmissionClass::Maintenance, "storage_squash_aspect"), post(maintain::squash_aspect));
	routes.route(Method::POST, "/api/v1/storage/{aspect}/compact", RouteSpec::stable(Scope::Maintain, Audit::AttemptOutcome, true, AdmissionClass::Maintenance, "storage_compact_aspect"), post(maintain::compact_aspect));
	routes.route(Method::POST, "/api/v1/storage/fsck", RouteSpec::stable(Scope::Maintain, Audit::AttemptOutcome, true, AdmissionClass::Maintenance, "storage_fsck"), post(maintain::fsck_store));
	routes.route(Method::GET, "/api/v1/storage/quarantine", RouteSpec::stable(Scope::Maintain, Audit::AttemptOutcome, true, AdmissionClass::Maintenance, "storage_quarantine_list"), get(maintain::quarantine_list));
	routes.route(Method::POST, "/api/v1/storage/quarantine/purge-expired", RouteSpec::stable(Scope::Maintain, Audit::AttemptOutcome, true, AdmissionClass::Maintenance, "storage_quarantine_purge_expired"), post(maintain::purge_expired_quarantine));
	routes.route(Method::POST, "/api/v1/storage/quarantine/{aspect}/{id}/{gen}/release", RouteSpec::stable(Scope::Maintain, Audit::AttemptOutcome, true, AdmissionClass::Maintenance, "storage_quarantine_release"), post(maintain::quarantine_release));
	routes.route(Method::POST, "/api/v1/storage/quarantine/{aspect}/{id}/{gen}/purge", RouteSpec::stable(Scope::Maintain, Audit::AttemptOutcome, true, AdmissionClass::Maintenance, "storage_quarantine_purge"), post(maintain::quarantine_purge));
}

/// Backup and the restore drill: whole-store operator routes.
fn register_backup_routes(routes: &mut Routes) {
	routes.route(Method::POST, "/api/v1/storage/backup", RouteSpec::stable(Scope::Backup, Audit::AttemptOutcome, true, AdmissionClass::Maintenance, "storage_backup"), post(backup_routes::backup_store));
	routes.route(Method::POST, "/api/v1/storage/restore/drill", RouteSpec::stable(Scope::Backup, Audit::AttemptOutcome, true, AdmissionClass::Maintenance, "storage_restore_drill"), post(backup_routes::restore_drill));
}

#[cfg(test)]
mod tests {
	use axum::{
		body::Body, http::{Request, StatusCode}
	};
	use tower::ServiceExt;

	use super::*;

	/// The scope table is frozen: eight scopes, these names, this order.
	/// A change here is a 1.0 policy change and must arrive as one, with
	/// the plan amended — not as a side effect of adding a route.
	#[test]
	fn scope_table_is_frozen() {
		let names: Vec<&str> = Scope::ALL.iter().map(|scope| scope.as_str()).collect();
		assert_eq!(names, ["probe", "metrics", "read", "compute", "ingest", "declare", "maintain", "backup"]);
	}

	/// Six roles, cumulative grants, and the admin holds every scope.
	#[test]
	fn role_table_is_frozen_and_cumulative() {
		let names: Vec<&str> = Role::ALL.iter().map(|role| role.as_str()).collect();
		assert_eq!(names, ["anonymous", "reader", "writer", "declarer", "maintainer", "admin"]);
		for pair in Role::ALL.windows(2) {
			let (lower, higher) = (pair[0], pair[1]);
			assert!(lower.scopes().iter().all(|scope| higher.allows(*scope)), "{higher:?} must hold everything {lower:?} holds");
		}
		assert!(Role::Admin.scopes().iter().all(|scope| Scope::ALL.contains(scope)));
		assert_eq!(Role::Anonymous.scopes(), &[Scope::Probe]);
		assert!(!Role::Reader.allows(Scope::Ingest));
		assert!(Role::Writer.allows(Scope::Ingest));
		assert!(!Role::Writer.allows(Scope::Declare));
		assert!(!Role::Maintainer.allows(Scope::Backup));
		assert!(Role::Admin.allows(Scope::Backup));
	}

	/// Every registered route is one the pre-SEC-3 router served: the
	/// registry must not lose (or invent) a route in the move. The count
	/// is 60 method routes over 54 path templates (55/49 before D-S12
	/// added the fsck and quarantine routes).
	#[test]
	fn registry_covers_the_whole_surface() {
		let table = build_registry().table();
		assert_eq!(table.entries().len(), 60, "the registry must carry every route the old lib.rs chain served");
		let mut operations: Vec<&str> = table.entries().iter().map(|entry| entry.spec.operation_id).collect();
		operations.sort_unstable();
		operations.dedup();
		assert_eq!(operations.len(), 60, "operation ids are unique");
		// Every scope in the frozen table is used by at least one route:
		// an unused scope in a frozen table is a scope nobody can hold.
		for scope in Scope::ALL {
			assert!(table.entries().iter().any(|entry| entry.spec.scope == scope), "scope {} has no route", scope.as_str());
		}
		// Spot-check the plan's own scope assignments.
		assert_eq!(table.by_operation("get_metrics").unwrap().spec.scope, Scope::Metrics);
		assert_eq!(table.by_operation("storage_reconcile_store").unwrap().spec.scope, Scope::Maintain);
		assert_eq!(table.by_operation("storage_ingest_ilp").unwrap().spec.scope, Scope::Ingest);
		assert_eq!(table.by_operation("storage_aspect_declare").unwrap().spec.scope, Scope::Declare);
	}

	/// The four products agree because they are one registry: the policy
	/// table and the class table have one row per route, and the OpenAPI
	/// document carries every operation id with its scope.
	#[test]
	fn one_registry_produces_policy_class_and_openapi() {
		let table = build_registry().table();
		assert_eq!(table.policy_table().len(), table.entries().len());
		assert_eq!(table.class_table().len(), table.entries().len());
		assert!(table.policy_table().iter().all(|row| table.by_operation(row.operation_id).is_some_and(|entry| entry.spec.scope == row.scope && entry.method == row.method && entry.path == row.path)));
		assert!(table.class_table().iter().all(|row| table.by_operation(row.operation_id).is_some_and(|entry| entry.spec.admission_class == row.admission_class && entry.spec.tier == row.tier)));

		let document = table.openapi_document();
		assert_eq!(document["openapi"], "3.1.0");
		let paths = document["paths"].as_object().unwrap();
		assert_eq!(paths.len(), 54, "one path item per template");
		let health = &paths["/health"]["get"];
		assert_eq!(health["operationId"], "get_health");
		assert_eq!(health["x-weft-scope"], "probe");
		let declare = &paths["/api/v1/storage/aspects"]["post"];
		assert_eq!(declare["operationId"], "storage_aspect_declare");
		assert_eq!(declare["x-weft-audit"], "attempt_outcome");
		// Both verbs on a dual-method path appear under one path item.
		assert_eq!(paths["/api/v1/storage/{aspect}/points"]["get"]["operationId"], "storage_points_list");
		assert_eq!(paths["/api/v1/storage/{aspect}/points"]["post"]["operationId"], "storage_ingest_points");
	}

	/// Registering the same (method, path) twice is a startup panic, not
	/// a silently shadowed route.
	#[test]
	#[should_panic(expected = "duplicate route")]
	fn duplicate_route_registration_panics() {
		let spec = RouteSpec::stable(Scope::Probe, Audit::None, false, AdmissionClass::Probe, "get_health_again");
		let mut routes = Routes::new();
		routes.route(Method::GET, "/health", RouteSpec::stable(Scope::Probe, Audit::None, false, AdmissionClass::Probe, "get_health"), get(health));
		routes.route(Method::GET, "/health", spec, get(health));
	}

	/// Registering the same operation id twice is a startup panic: the
	/// id is a key in three of the four products.
	#[test]
	#[should_panic(expected = "duplicate operation_id")]
	fn duplicate_operation_id_panics() {
		let mut routes = Routes::new();
		routes.route(Method::GET, "/health", RouteSpec::stable(Scope::Probe, Audit::None, false, AdmissionClass::Probe, "same_operation"), get(health));
		routes.route(Method::GET, "/ready", RouteSpec::stable(Scope::Probe, Audit::None, false, AdmissionClass::Probe, "same_operation"), get(ready));
	}

	async fn send(method: &str, uri: &str) -> (StatusCode, Option<String>, serde_json::Value) {
		let response = crate::app().oneshot(Request::builder().method(method).uri(uri).body(Body::empty()).unwrap()).await.unwrap();
		let status = response.status();
		let allow = response.headers().get(header::ALLOW).map(|value| value.to_str().unwrap().to_string());
		let bytes = axum::body::to_bytes(response.into_body(), usize::MAX).await.unwrap();
		(status, allow, serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null))
	}

	/// Fail-closed, unknown path: the fallback refuses with the envelope's
	/// 404, as before SEC-3.
	#[tokio::test]
	async fn unregistered_path_is_refused_404() {
		let (status, _, body) = send("GET", "/nope").await;
		assert_eq!(status, StatusCode::NOT_FOUND);
		assert_eq!(body["code"], "route_not_found");
	}

	/// Fail-closed, known path under the wrong method: the gate refuses
	/// with 405 and an `Allow` header naming the registered method —
	/// FRE-6a's 405 fallback, which axum's bare 405 never gave.
	#[tokio::test]
	async fn wrong_method_on_registered_path_is_refused_405() {
		let (status, allow, body) = send("DELETE", "/health").await;
		assert_eq!(status, StatusCode::METHOD_NOT_ALLOWED);
		assert_eq!(allow.as_deref(), Some("GET"));
		assert_eq!(body["code"], "method_not_allowed");
	}

	/// The same 405 through a path template: the fallback matches the
	/// concrete path against the registry's templates.
	#[tokio::test]
	async fn wrong_method_on_templated_path_is_refused_405() {
		let (status, allow, body) = send("DELETE", "/api/v1/storage/price/points").await;
		assert_eq!(status, StatusCode::METHOD_NOT_ALLOWED);
		let allow = allow.unwrap();
		assert!(allow.contains("GET") && allow.contains("POST"), "Allow names both registered methods, got {allow}");
		assert_eq!(body["code"], "method_not_allowed");
	}

	/// A registered route still serves through the gate.
	#[tokio::test]
	async fn registered_route_serves_through_the_gate() {
		let (status, _, body) = send("GET", "/health").await;
		assert_eq!(status, StatusCode::OK);
		assert_eq!(body["status"], "ok");
	}
}
