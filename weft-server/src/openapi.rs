//! OpenAPI 3.1 document generation from the route registry (FRE-9a).
//!
//! The document is built from the same [`RouteTable`] the router serves, so
//! every path, method, scope, admission class and tier that the wire presents
//! is the one the code registered. Adding a route changes the generated
//! document; a test asserts the committed spec matches it (the drift gate).
//! FRE-9b (W11) publishes and extends the document.
//!
//! ## Subcommand
//!
//! ```text
//! weft-server openapi [--out PATH]
//! ```
//!
//! With no `--out`, or `--out -`, the document is written to stdout. With
//! `--out <path>`, it is written to that file. The command refuses a directory
//! or an unwritable path with exit 78.

use std::collections::BTreeMap;

use axum::http::Method;
use serde_json::{json, Map, Value};

use crate::routes::{RouteSpec, RouteTable};

/// The first argv word that selects this command instead of serving.
pub const SUBCOMMAND: &str = "openapi";

/// The HTTP status codes the server's error table can produce, derived from
/// `Code::status` in `error.rs`.
pub const ERROR_STATUSES: &[u16] = &[400, 401, 403, 404, 405, 408, 409, 410, 413, 415, 422, 500, 503, 504, 507];

const fn error_status_description(status: u16) -> &'static str {
	match status {
		400 => "Bad Request — the request body or query is malformed",
		401 => "Unauthenticated — the request carries no `Authorization` header, or the token is invalid",
		403 => "Forbidden — the caller's key does not hold the scope this route requires",
		404 => "Not Found — no route matches the path, or the named resource does not exist",
		405 => "Method Not Allowed — the path exists but not under this HTTP method",
		408 => "Request Timeout — the client did not send the body within the server's configured deadline",
		409 => "Conflict — the request would create a duplicate, a schema mismatch, or the resource is busy",
		410 => "Gone — the cursor the request refers to is stale and cannot be advanced",
		413 => "Payload Too Large — the request body exceeds the server's configured limit",
		415 => "Unsupported Media Type — the `Content-Type` is not one the route accepts",
		422 => "Unprocessable Entity — the result exceeds the server's configured output limit",
		500 => "Internal Server Error — the server could not complete the request",
		503 => "Service Unavailable — a subsystem is not configured, not ready, overloaded, or poisoned",
		504 => "Gateway Timeout — the operation did not finish within its configured deadline",
		507 => "Insufficient Storage — the filesystem cannot safely accept the write",
		_ => "unexpected error",
	}
}

/// Derive a human-readable summary from a snake_case `operation_id`:
/// split on `_`, capitalise the first word, and join with spaces.
fn operation_summary(id: &str) -> String {
	let mut words = id.split('_').filter(|word| !word.is_empty());
	let Some(first) = words.next() else {
		return id.to_string();
	};
	let mut buf = String::new();
	let mut chars = first.chars();
	if let Some(head) = chars.next() {
		buf.extend(head.to_uppercase());
	}
	buf.extend(chars);
	for word in words {
		buf.push(' ');
		buf.push_str(word);
	}
	buf
}

/// Derive a tag from the operation id, grouping operations by API area.
fn operation_tag(id: &str) -> &'static str {
	if id.starts_with("get_health") || id.starts_with("get_ready") {
		"Probes"
	} else if id == "get_version" {
		"Version"
	} else if id.starts_with("get_metrics") || id.starts_with("get_debug") {
		"Metrics"
	} else if id.starts_with("storage_") {
		if id.contains("ingest") {
			"Ingest"
		} else if id.starts_with("storage_aspect_declare") {
			"Declare"
		} else if id.contains("reconcile") || id.contains("quarantine") || id.contains("squash") || id.contains("compact") || id.contains("fsck") {
			"Maintenance"
		} else if id.contains("backup") || id.contains("restore") {
			"Backup"
		} else {
			"Storage Reads"
		}
	} else if id.starts_with("interpolate") || id.starts_with("downsample") {
		"Compute"
	} else if id.starts_with("grafana") {
		"Grafana"
	} else if id.starts_with("list_bench") || id.starts_with("get_bench") {
		"Benchmarks"
	} else {
		"Other"
	}
}

/// The OpenAPI method key (lowercase) for an HTTP method.
const fn method_key(method: &Method) -> &'static str {
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

/// Build the `ErrorEnvelope` schema entry.
fn error_envelope_schema() -> Value {
	json!({
		"type": "object",
		"description": "The standard error envelope (plan §4). Every error response carries this shape; `code` names the exact error, `message` is a human-readable description, and `details` carries structured context when present.",
		"properties": {
			"code": { "type": "string", "description": "The §4 error code, e.g. `bad_request`, `route_not_found`, `internal`" },
			"message": { "type": "string", "description": "A human-readable description of what went wrong" },
			"details": { "type": "object", "description": "Structured context specific to the error (absent when empty)", "nullable": true },
			"request_id": { "type": "string", "description": "The opaque request id, present in `5xx` responses" },
			"limit": { "type": "integer", "description": "The server-side cap that was exceeded (present for rate-limited or size-limited errors)", "nullable": true },
			"committed": { "type": "boolean", "description": "Whether the write was committed despite the error (present for outcome-unknown commits)", "nullable": true },
			"retry_after": { "type": "integer", "description": "The suggested seconds before retrying (present for overload/busy errors)", "nullable": true },
		},
	})
}

/// Build the `responses` entry for one error status.
fn error_response_entry(status: u16) -> Value {
	let desc = error_status_description(status);
	json!({
		"description": desc,
		"content": {
			"application/json": {
				"schema": { "$ref": "#/components/schemas/ErrorEnvelope" },
			},
		},
	})
}

/// Build one operation object for a route entry's spec.
fn build_operation(spec: &RouteSpec) -> Value {
	let summary = operation_summary(spec.operation_id);
	let tag = operation_tag(spec.operation_id);

	let mut responses: Map<String, Value> = Map::new();
	responses.insert("200".to_string(), json!({"description": "the operation completed successfully"}));
	for &status in ERROR_STATUSES {
		responses.insert(status.to_string(), error_response_entry(status));
	}

	let mut result = Map::new();
	result.insert("operationId".to_string(), Value::String(spec.operation_id.to_string()));
	result.insert("summary".to_string(), Value::String(summary));
	result.insert("tags".to_string(), Value::Array(vec![Value::String(tag.to_string())]));
	result.insert("responses".to_string(), Value::Object(responses));
	result.insert("x-weft-scope".to_string(), Value::String(spec.scope.as_str().to_string()));
	result.insert("x-weft-audit".to_string(), Value::String(spec.audit.as_str().to_string()));
	result.insert("x-weft-operator-detail".to_string(), json!(spec.operator_detail));
	result.insert("x-weft-tier".to_string(), Value::String(spec.tier.label().to_string()));
	result.insert("x-weft-admission-class".to_string(), Value::String(spec.admission_class.as_str().to_string()));
	Value::Object(result)
}

/// Extract path parameters from a path template: segments wrapped in
/// `{` `}` become OpenAPI path-parameter entries.
fn extract_path_parameters(template: &str) -> Vec<Value> {
	let mut params: Vec<Value> = Vec::new();
	for segment in template.split('/') {
		if segment.starts_with('{') && segment.ends_with('}') {
			let name: &str = &segment[1..segment.len() - 1];
			params.push(json!({
				"name": name,
				"in": "path",
				"required": true,
				"schema": { "type": "string" },
			}));
		}
	}
	params
}

/// Build the `components` section.
fn build_components() -> Value {
	let mut schemas: Map<String, Value> = Map::new();
	schemas.insert("ErrorEnvelope".to_string(), error_envelope_schema());

	let mut responses: Map<String, Value> = Map::new();
	for &status in ERROR_STATUSES {
		let key = format!("Error{status}");
		responses.insert(key, error_response_entry(status));
	}

	json!({
		"schemas": schemas,
		"responses": responses,
	})
}

/// Generate the full OpenAPI 3.1 document from a sealed route table.
///
/// Every path item, operation, path parameter, scope and admission class
/// comes from the same registrations the running server uses.
#[must_use]
pub fn from_table(table: &RouteTable) -> Value {
	let mut path_groups: BTreeMap<&'static str, Map<String, Value>> = BTreeMap::new();
	for entry in table.entries() {
		let methods = path_groups.entry(entry.path).or_default();
		let operation = build_operation(&entry.spec);
		methods.insert(method_key(&entry.method).to_string(), operation);
	}

	// A BTreeMap iterates in key order, so the document is deterministic
	// without sorting the paths a second time.
	let mut path_items: Map<String, Value> = Map::new();
	for (path, methods) in &path_groups {
		let mut item: Map<String, Value> = Map::new();
		for (method, operation) in methods {
			item.insert(method.clone(), operation.clone());
		}
		// Inject path parameters from the template.
		let params = extract_path_parameters(*path);
		if !params.is_empty() {
			item.insert("parameters".to_string(), Value::Array(params));
		}
		path_items.insert((*path).to_string(), Value::Object(item));
	}

	json!({
		"openapi": "3.1.0",
		"info": {
			"title": crate::SERVICE,
			"version": crate::VERSION,
		},
		"paths": path_items,
		"components": build_components(),
	})
}

// ── structural validation ──────────────────────────────────────────────────

/// Validate the structural integrity of an OpenAPI document.
///
/// Returns a list of issues found: an empty list means the document
/// passes all checks.
#[must_use]
pub fn validate(document: &Value) -> Vec<String> {
	let mut issues: Vec<String> = Vec::new();
	// A missing section is something to report, not something to panic on, so
	// every `as_object` below borrows this map instead of a temporary.
	let empty = Map::new();

	// 1. OpenAPI version is 3.1.x
	let version = document["openapi"].as_str().unwrap_or("");
	if !version.starts_with("3.1") {
		issues.push(format!("`openapi` version must start with `3.1`, got {version:?}"));
	}

	let components = document["components"].as_object().unwrap_or(&empty);
	let schemas = components["schemas"].as_object().unwrap_or(&empty);
	let comp_responses = components["responses"].as_object().unwrap_or(&empty);

	let paths = document["paths"].as_object().unwrap_or(&empty);
	let mut operation_ids: Vec<String> = Vec::new();

	for (ppath, item) in paths {
		let Some(methods) = item.as_object() else {
			issues.push(format!("path `{ppath}` is not an object"));
			continue;
		};
		let method_keys: Vec<&str> = methods.keys().map(String::as_str).filter(|key| *key != "parameters").collect();

		if method_keys.is_empty() {
			issues.push(format!("path `{ppath}` has no operations"));
			continue;
		}

		for method in method_keys {
			let Some(operation) = methods[method].as_object() else {
				issues.push(format!("`{ppath}`/{method} is not an object"));
				continue;
		};

			// 3a. Has operationId.
			let op_id = operation["operationId"].as_str().unwrap_or("");
			if op_id.is_empty() {
				issues.push(format!("`{ppath}`/{method} has no `operationId`"));
			} else {
				operation_ids.push(op_id.to_string());
			}

			// 3b. Has summary.
			if operation["summary"].as_str().unwrap_or("").is_empty() {
				issues.push(format!("`{ppath}`/{method} has no `summary`"));
			}

			// 3c. Has a 2xx response.
			let responses = operation["responses"].as_object().unwrap_or(&empty);
			let has_2xx = responses.keys().any(|k| k.starts_with('2'));
			if !has_2xx {
				issues.push(format!("`{ppath}`/{method} has no `2xx` response"));
			}

			// 3d. Has at least one documented error response.
			let has_error = responses.keys().any(|k| k.starts_with('4') || k.starts_with('5'));
			if !has_error {
				issues.push(format!("`{ppath}`/{method} has no documented error response"));
			}
		}
	}

	// 4. Every $ref target exists in components.
	collect_refs(document, &mut issues, schemas, comp_responses);

	// 5. No two operations share an operationId.
	operation_ids.sort();
	for i in 1..operation_ids.len() {
		if operation_ids[i] == operation_ids[i - 1] {
			issues.push(format!("duplicate `operationId`: `{}`", operation_ids[i]));
		}
	}

	issues
}

/// Collect every `$ref` in the document and check that its target exists.
fn collect_refs(value: &Value, issues: &mut Vec<String>, schemas: &Map<String, Value>, responses: &Map<String, Value>) {
	match value {
		Value::Object(obj) => {
			for (key, val) in obj {
				if key == "$ref" {
					if let Some(ref_str) = val.as_str() {
						check_ref(ref_str, issues, schemas, responses);
					}
				} else {
					collect_refs(val, issues, schemas, responses);
				}
			}
		}
		Value::Array(arr) => {
			for val in arr {
				collect_refs(&val, issues, schemas, responses);
			}
		}
		_ => {}
	}
}

/// Check that a single `$ref` string targets an existing component.
fn check_ref(target: &str, issues: &mut Vec<String>, schemas: &Map<String, Value>, responses: &Map<String, Value>) {
	if !target.starts_with("#/components/") {
		issues.push(format!("`{target}` is not a valid `$ref` (must start with `#/components/`)"));
		return;
	}
	let tail = target.strip_prefix("#/components/").unwrap_or("");
	// tail is "schemas/Foo" or "responses/Error400"
	let Some((category, name)) = tail.split_once('/') else {
		issues.push(format!("`{target}` has an unrecognised component path"));
		return;
	};
	if category.is_empty() || name.is_empty() {
		issues.push(format!("`{target}` has an unrecognised component path"));
		return;
	}
	match category {
		"schemas" => {
			if !schemas.contains_key(name) {
				issues.push(format!("`$ref` target `{name}` not found in `components/schemas`"));
			}
		}
		"responses" => {
			if !responses.contains_key(name) {
				issues.push(format!("`$ref` target `{name}` not found in `components/responses`"));
			}
		}
		_ => {
			issues.push(format!("`$ref` references unknown component category `{category}`"));
		}
	}
}

// ── CLI subcommand ─────────────────────────────────────────────────────────

/// Run the `openapi` subcommand: generate the document and write it.
///
/// `arguments` follows the program name, starting with the subcommand word
/// `openapi`. Returns a usage error on a parse refusal, or propagates an
/// I/O error.
pub fn run(arguments: Vec<String>) -> anyhow::Result<()> {
	use std::io::Write as _;
	let out_path = parse_args(arguments)?;

	let table = crate::routes::build_registry().table();
	let document = from_table(&table);

	// Pretty-printed with a trailing newline: the committed document is diffed
	// by hand as well as by the drift gate.
	let mut json_bytes = serde_json::to_vec_pretty(&document)?;
	json_bytes.push(b'\n');

	match out_path.as_deref() {
		None | Some("-") => {
			std::io::stdout().write_all(&json_bytes).map_err(|error| anyhow::anyhow!("openapi: writing the document to stdout failed: {error}"))?;
		}
		Some(path) => {
			std::fs::write(path, &json_bytes).map_err(|error| anyhow::anyhow!("openapi: writing the document to {path} failed: {error}"))?;
		}
	}
	Ok(())
}

/// Parse the `openapi` subcommand's arguments.
///
/// Accepted forms:
/// - `openapi` → stdout
/// - `openapi --out -` → stdout
/// - `openapi --out <path>` → write to file
fn parse_args(mut arguments: Vec<String>) -> Result<Option<String>, crate::cli::UsageError> {
	// `arguments[0]` is the `openapi` word the parser already matched.
	if arguments.len() <= 1 {
		return Ok(None);
	}
	let first = arguments[1].clone();
	match first.as_str() {
		"--out" => {
			if arguments.len() <= 2 {
				return Err(crate::cli::usage("openapi: `--out` needs a path argument; use `-` for stdout"));
			}
			if arguments.len() > 3 {
				return Err(crate::cli::usage(&format!("openapi: `{}` is not an argument of `--out`; usage: weft-server openapi [--out PATH]", arguments[3])));
			}
			let path = arguments.remove(2);
			if std::path::Path::new(&path).is_dir() {
				return Err(crate::cli::usage(&format!("openapi: `{path}` is a directory; `--out` expects a file path or `-` for stdout")));
			}
			Ok(Some(path))
		}
		"--help" | "-h" => Err(crate::cli::usage("openapi: generate the OpenAPI 3.1 document from the route registry\n\nusage: weft-server openapi [--out PATH]\n\nWith no `--out`, or `--out -`, the document is written to stdout.\nWith `--out <path>`, it is written to that file.\n\nThe command exits 0 on success and 78 on a refusal.")),
		other if other.starts_with('-') => Err(crate::cli::usage(&format!("openapi: unrecognised option `{other}`; only `--out PATH` is accepted, with `-` for stdout"))),
		other => Err(crate::cli::usage(&format!("openapi: unrecognised argument `{other}`"))),
	}
}

#[cfg(test)]
mod tests {
	use super::*;
	use crate::routes::build_registry;

	/// The document generated from the registry the server actually serves.
	fn generated() -> Value {
		from_table(&build_registry().table())
	}

	/// The drift gate: the committed spec is the document the routes generate.
	/// A generated file nothing compares against is a second source of truth,
	/// which is what this slice exists to prevent.
	#[test]
	fn generated_spec_matches_committed_file() {
		let committed_path = "../compat/openapi/weftdb.json";
		if !std::path::Path::new(committed_path).exists() {
			panic!(
				"`{committed_path}` is missing; the drift gate needs a committed spec file.\n\t\
			 Generate it from the `weft-server/` crate directory with:\n\t\
			 \t cargo run -p weft-server -- openapi --out ../compat/openapi/weftdb.json"
			);
		}
		let text = std::fs::read_to_string(committed_path).unwrap_or_else(|error| panic!("`{committed_path}` could not be read: {error}"));
		let committed: Value = serde_json::from_str(&text).unwrap_or_else(|error| panic!("`{committed_path}` is not valid JSON: {error}"));
		let generated = generated();
		if generated != committed {
			let generated_paths = generated["paths"].as_object().map_or(0, |items| items.len());
			let committed_paths = committed["paths"].as_object().map_or(0, |items| items.len());
			panic!(
				"the committed spec at `{committed_path}` is not what the routes generate \n\t\
			 ({generated_paths} paths generated, {committed_paths} committed).\n\t\
			 Regenerate it with: cargo run -p weft-server -- openapi --out ../compat/openapi/weftdb.json"
			);
		}
	}

	/// Structural validation of the generated document: every path item carries
	/// an operation, every operation a 2xx and an error response, every `$ref`
	/// an existing target, and no two operations a shared `operationId`.
	#[test]
	fn generated_spec_passes_structural_validation() {
		let issues = validate(&generated());
		if !issues.is_empty() {
			panic!("the generated OpenAPI document has structural issues:\n  {}", issues.join("\n  "));
		}
	}

	/// Every status the error table can produce has a documented component
	/// response, so a client is never handed a status with no shape.
	#[test]
	fn every_error_status_has_a_component_response() {
		let document = generated();
		let components = document["components"].as_object().expect("the document has a components section");
		let responses = components.get("responses").and_then(Value::as_object).expect("components has a responses section");
		for status in ERROR_STATUSES {
			let key = format!("Error{status}");
			let entry = responses.get(&key).unwrap_or_else(|| panic!("`{key}` is missing from `components.responses`"));
			assert!(entry["description"].as_str().is_some_and(|text| !text.is_empty()), "`{key}` has no description");
		}
	}

	/// The committed file is a documented API, not a placeholder: an empty
	/// `paths` object would satisfy a byte-comparison drift gate the day the
	/// routes regenerate onto it, and would ship a spec that documents nothing.
	#[test]
	fn committed_spec_is_a_documented_api() {
		let path = "../compat/openapi/weftdb.json";
		let text = std::fs::read_to_string(path).unwrap_or_else(|error| panic!("`{path}` must exist: generate it with `cargo run -p weft-server -- openapi --out ../compat/openapi/weftdb.json`: {error}"));
		let parsed: Value = serde_json::from_str(&text).unwrap_or_else(|error| panic!("`{path}` is not valid JSON: {error}"));
		let paths = parsed["paths"].as_object().unwrap_or_else(|| panic!("`{path}` has no `paths` object"));
		assert!(!paths.is_empty(), "`{path}` documents no paths: the committed spec is a placeholder");
	}

	/// The version is the one the document claims to be, not a bare "3.1".
	#[test]
	fn openapi_version_is_3_1() {
		let document = generated();
		assert_eq!(document["openapi"], "3.1.0");
	}

	/// The document names the service and the version that produced it.
	#[test]
	fn info_block_has_service_name_and_version() {
		let document = generated();
		let info = document["info"].as_object().expect("the document has an info block");
		assert_eq!(info["title"], crate::SERVICE);
		assert!(!info["version"].as_str().unwrap_or_default().is_empty(), "the version must not be empty");
	}

	/// One path item per registered template, counted from the registry rather
	/// than hardcoded, so adding a route cannot pass against a stale number.
	#[test]
	fn one_path_item_per_registered_template() {
		let table = build_registry().table();
		let mut templates: Vec<&str> = table.entries().iter().map(|entry| entry.path).collect();
		templates.sort_unstable();
		templates.dedup();
		assert!(!templates.is_empty(), "the registry registered no routes at all");
		let document = generated();
		let paths = document["paths"].as_object().expect("the document has a paths section");
		assert_eq!(paths.len(), templates.len(), "one path item per registered template");
	}

	/// No two operations share an `operationId`: it is the client's method name.
	#[test]
	fn no_duplicate_operation_ids() {
		let duplicates: Vec<String> = validate(&generated()).into_iter().filter(|issue| issue.contains("duplicate `operationId`")).collect();
		assert!(duplicates.is_empty(), "duplicate operationIds: {duplicates:?}");
	}

	/// Every operation carries the summary and id a generated client needs.
	#[test]
	fn every_operation_has_a_summary_and_an_id() {
		let document = generated();
		let paths = document["paths"].as_object().expect("the document has a paths section");
		for (path, item) in paths {
			let methods = item.as_object().unwrap_or_else(|| panic!("`{path}` is not an object"));
			for (method, operation) in methods {
				if method == "parameters" {
					continue;
				}
				assert!(!operation["summary"].as_str().unwrap_or_default().is_empty(), "operation `{path}`/{method} has no summary");
				assert!(!operation["operationId"].as_str().unwrap_or_default().is_empty(), "operation `{path}`/{method} has no operationId");
			}
		}
	}
}
