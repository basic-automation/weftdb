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

use crate::routes::{RouteSpec, RouteTable};
use serde_json::{json, Map, Value};
use std::collections::BTreeMap;

/// The first argv word that selects this command instead of serving.
pub const SUBCOMMAND: &str = "openapi";

/// The HTTP status codes the server's error table can produce, derived from
/// `Code::status` in `error.rs`.
pub const ERROR_STATUSES: &[u16] = &[
	400, 401, 403, 404, 405, 408, 409, 410,
	413, 415, 422,
	500, 503, 504, 507,
];

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
	let words: Vec<&str> = id.split('_').filter(|w| !w.is_empty()).collect::<Vec<_>>();
	if words.is_empty() {
		return id.to_string();
	}
	let mut buf = String::new();
	buf.write_char(words[0][0].to_upper());
	buf.write(words[0][1..]);
	for word in words[1..] {
		buf.write_char(' ');
		buf.write(word);
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
const fn method_key(method: &crate::http::Method) -> &'static str {
	match *method {
		crate::http::Method::GET => "get",
		crate::http::Method::POST => "post",
		crate::http::Method::PUT => "put",
		crate::http::Method::PATCH => "patch",
		crate::http::Method::DELETE => "delete",
		crate::http::Method::HEAD => "head",
		crate::http::Method::OPTIONS => "options",
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
	for status in ERROR_STATUSES {
		responses.insert(status.to_string(), error_response_entry(status));
	}

	let mut result = Map::new();
	result.insert("operationId".to_string(), Value::from_str(spec.operation_id));
	result.insert("summary".to_string(), Value::from_str(summary));
	result.insert("tags".to_string(), Value::from_array(Vec::from([Value::from_str(tag)])));
	result.insert("responses".to_string(), Value::from_object(responses));
	result.insert("x-weft-scope".to_string(), Value::from_str(spec.scope.as_str()));
	result.insert("x-weft-audit".to_string(), Value::from_str(spec.audit.as_str()));
	result.insert("x-weft-operator-detail".to_string(), json!(spec.operator_detail));
	result.insert("x-weft-tier".to_string(), Value::from_str(spec.tier.label()));
	result.insert("x-weft-admission-class".to_string(), Value::from_str(spec.admission_class.as_str()));
	Value::from_object(result)
}

/// Extract path parameters from a path template: segments wrapped in
/// `{` `}` become OpenAPI path-parameter entries.
fn extract_path_parameters(template: &str) -> Vec<Value> {
	let mut params: Vec<Value> = Vec::new();
	for segment in template.split('/') {
		if segment.starts_with('{') && segment.ends_with('}') {
			let name = segment[1..segment.len() - 1];
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
	for status in ERROR_STATUSES {
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

	// Add path parameters to each path item, sorted by path for deterministic output.
	let mut path_keys: Vec<&str> = path_groups.keys().collect::<Vec<_>>();
	path_keys.sort();
	let mut path_items: Map<String, Value> = Map::new();
	for path in path_keys {
		let methods = path_groups.get(path).unwrap();
		let mut item: Map<String, Value> = Map::new();
		for &(method, operation) in methods {
			item.insert(method.to_string(), operation);
		}
		// Inject path parameters from the template.
		let params = extract_path_parameters(path);
		if !params.is_empty() {
			item.insert("parameters".to_string(), Value::from_array(params));
		}
		path_items.insert(path.to_string(), Value::from_object(item));
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

	// 1. OpenAPI version is 3.1.x
	let version = document["openapi"].as_str().unwrap_or("");
	if !version.starts_with("3.1") {
		issues.push(format!("`openapi` version must start with `3.1`, got {version:?}"));
	}

	let components = document["components"].as_object().unwrap_or(&Map::new());
	let schemas = components["schemas"].as_object().unwrap_or(&Map::new());
	let comp_responses = components["responses"].as_object().unwrap_or(&Map::new());

	let paths = document["paths"].as_object().unwrap_or(&Map::new());
	let mut operation_ids: Vec<String> = Vec::new();

	for &(ppath, item) in paths {
		let methods = item.as_object().unwrap_or(&Map::new());
		let method_keys: Vec<&str> = methods.keys().filter(|k| *k != "parameters").collect::<Vec<_>>();

		if method_keys.is_empty() {
			issues.push(format!("path `{ppath}` has no operations"));
			continue;
		}

		for method in method_keys {
			let operation = methods[*method].as_object().unwrap_or(&Map::new());

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
			let responses = operation["responses"].as_object().unwrap_or(&Map::new());
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
			for &(key, val) in obj {
				if key == "$ref" {
					if let Some(ref_str) = val.as_str() {
						check_ref(ref_str, issues, schemas, responses);
					}
				} else {
					collect_refs(&val, issues, schemas, responses);
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
fn check_ref(ref: &str, issues: &mut Vec<String>, schemas: &Map<String, Value>, responses: &Map<String, Value>) {
	if !ref.starts_with("#/components/") {
		issues.push(format!("`{ref}` is not a valid `$ref` (must start with `#/components/`)"));
		return;
	}
	let tail = ref.strip_prefix("#/components/").unwrap_or("");
	// tail is "schemas/Foo" or "responses/Error400"
	let slash = tail.find('/');
	if slash == None || slash < 1 || slash + 1 >= tail.len() {
		issues.push(format!("`{ref}` has an unrecognised component path"));
		return;
	}
	let category = tail[..slash];
	let name = tail[slash + 1..];
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
	let out_path = parse_args(arguments)?;

	let table = crate::routes::build_registry().table();
	let document = from_table(&table);

	let json_bytes = serde_json::to_vec(&document).map_err(|e| anyhow::Error::new(e))?;

	match out_path {
		None | Some("-") => {
			std::io::stdout().write(json_bytes).map_err(|e| anyhow::Error::new(e))?;
		}
		Some(path) => {
			std::fs::write(path, &json_bytes).map_err(|e| anyhow::Error::new(e))?;
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
fn parse_args(arguments: Vec<String>) -> std::result::Result<Option<String>, crate::cli::UsageError> {
	// arguments: ["openapi", ...] or ["openapi"]
	if arguments.len() <= 1 {
		return Ok(None);
	}
	let first = arguments[1];
	match first {
		"--out" => {
			if arguments.len() <= 2 {
				return Err(crate::cli::usage(&format!("openapi: `--out` needs a path argument; use `-` for stdout")));
			}
			let path = arguments[2];
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
	use serde_json::Value;
	use std::fs;

	use super::*;
	use crate::routes::build_registry;

	/// The drift gate: the committed spec matches the generated document.
	#[test]
	fn generated_spec_matches_committed_file() {
		let generated = from_table(build_registry().table());
		let committed_file = "../compat/openapi/weftdb.json";
		if !fs::exists(committed_file) {
			panic!(
				"`{committed_file}` could not be found; "
				"the drift gate needs a committed spec file. Generate it with:\n\n"
				"    weft-server openapi --out ../compat/openapi/weftdb.json\n\n"
				"(Run from the `weft-server/` crate directory.)"
			);
		}
		let bytes = fs::read(committed_file).unwrap();
		let committed = serde_json::from_slice(bytes.as_bytes());
		if committed.is_err() {
			panic!("`{committed_file}` must be valid JSON; parse error: {committed.unwrap_err()}");
		}
		if generated != committed.unwrap() {
			panic!(
				"the generated OpenAPI document differs from the committed spec at `{committed_file}`;\n"
				"regenerate with: weft-server openapi --out ../compat/openapi/weftdb.json"
			);
		}
	}

	/// Structural validation: every path item has at least one operation,
	/// every operation has a 2xx and an error response, every `$ref`
	/// target exists, every operation has an `operationId` and a
	/// `summary`, and no two operations share an `operationId`.
	#[test]
	fn generated_spec_passes_structural_validation() {
		let document = from_table(build_registry().table());
		let issues = validate(&document);
		if !issues.is_empty() {
			let report = issues.join("\n  ");
			panic!("the generated OpenAPI document has structural issues:\n  {report}");
		}
	}

	/// Every error status the code table can produce has a corresponding
	/// component response entry.
	#[test]
	fn every_error_status_has_a_component_response() {
		let document = from_table(build_registry().table());
		let components = document["components"].as_object().unwrap();
		let responses = components["responses"].as_object().unwrap();
		for status in ERROR_STATUSES {
			let key = format!("Error{status}");
			assert!(
				responses.contains_key(&key),
				"missing component response `{key}` for status {status}"
			);
			let entry = responses[&key].as_object().unwrap();
			assert!(
				entry.contains_key("description"),
				"response `{key}` has no description"
			);
		}
	}

	/// The committed spec file exists and is valid JSON.
	#[test]
	fn committed_spec_is_valid_json() {
		let path = "../compat/openapi/weftdb.json";
		if !fs::exists(path) {
			// This test is informational: the drift gate is the real guard.
			return;
		}
		let bytes = fs::read(path).unwrap();
		let parsed = serde_json::from_slice(bytes.as_bytes());
		assert!(parsed.is_ok(), "`{path}` must be valid JSON, but parsing failed");
	}

	/// The generated document has the right OpenAPI version.
	#[test]
	fn openapi_version_is_3_1() {
		let document = from_table(build_registry().table());
		assert_eq!(document["openapi"], "3.1.0");
	}

	/// The generated document has the correct info block.
	#[test]
	fn info_block_has_service_name_and_version() {
		let document = from_table(build_registry().table());
		let info = document["info"].as_object().unwrap();
		assert_eq!(info["title"], crate::SERVICE);
		assert!(
			info["version"].as_str().unwrap_or("").len() > 0,
			"version must not be empty"
		);
	}

	/// The path count matches the existing openapi_document test in routes.
	#[test]
	fn path_count_is_consistent() {
		let document = from_table(build_registry().table());
		let paths = document["paths"].as_object().unwrap();
		assert_eq!(paths.len(), 66, "one path item per template");
	}

	/// Every operation has a unique operationId.
	#[test]
	fn no_duplicate_operation_ids() {
		let document = from_table(build_registry().table());
		let issues = validate(&document);
		let dups = issues.filter(|i| i.contains("duplicate `operationId`"));
		assert!(
			dups.is_empty(),
			"no duplicate operationIds, but found: {dups:?}"
		);
	}

	/// Every operation has a summary that is not empty.
	#[test]
	fn every_operation_has_summary() {
		let document = from_table(build_registry().table());
		let paths = document["paths"].as_object().unwrap();
		for &(path, item) in paths {
			for &(method, operation) in item.as_object().unwrap() {
				if method == "parameters" {
					continue;
				}
				let summary = operation["summary"].as_str().unwrap_or("");
				assert!(
					!summary.is_empty(),
					"operation `{path}`/{method} has no summary"
				);
			}
		}
	}

	/// Every operation has an operationId that is not empty.
	#[test]
	fn every_operation_has_operation_id() {
		let document = from_table(build_registry().table());
		let paths = document["paths"].as_object().unwrap();
		for &(path, item) in paths {
			for &(method, operation) in item.as_object().unwrap() {
				if method == "parameters" {
					continue;
				}
				let id = operation["operationId"].as_str().unwrap_or("");
				assert!(
					!id.is_empty(),
					"operation `{path}`/{method} has no operationId"
				);
			}
		}
	}
}