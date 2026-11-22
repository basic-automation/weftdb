#!/usr/bin/env python3
"""
check-openapi-breaking.py

Compare a live OpenAPI spec against a frozen baseline and classify every
difference against the six compatibility rule families documented in
``docs/stability.md:27-40``:

- Adding a route is additive (non-breaking) as long as existing routes are
  unchanged.
- Adding an optional field to a request body or response object is non-breaking.
- Adding a new query parameter that is silently ignored when absent is
  non-breaking.
- Removing or renaming a route, changing a required field to optional,
  tightening or loosening a type constraint, or adding a required query
  parameter is breaking.
- Changing the semantics of an existing response field — including making a
  field that was always present become nullable for part of the response —
  is **wire-breaking**: a client that assumed ``total`` was always a number
  must now handle ``null``.
- Adding a non-trivial default to a query parameter that previously had no
  default, if that default changes the behaviour of a request that omitted it,
  is breaking.

Known gaps (declared in ``docs/stability.md``):
- Top-level documents ``security`` / ``securitySchemes`` / ``servers`` / ``info`` /
  ``tags`` / ``externalDocs`` are out of scope (not compared).
- ``required`` array changes on nested objects that are already caught as property-level
  optional/required deltas are not restated here (the docstring is not a gap list).

Combinator member content (``allOf`` / ``oneOf`` / ``anyOf``) **is** now compared
(FRE-18c).

Usage:
    python3 scripts/ci/check-openapi-breaking.py [--baseline PATH] [--current PATH]

Exit codes:
    0 — no breaking changes found (diff is clean or additive-only)
    1 — one or more breaking findings
    2 — missing/unreadable/unparseable baseline or empty paths
"""

import argparse
import json
import os
import sys
from copy import deepcopy

# Sentinel used by the Rust drift gate to exclude version from comparison.
_VERSION_SENTINEL = "version::excluded-from-drift-comparison"

# Default file paths.
_HERE = os.path.dirname(os.path.abspath(__file__))
_DEFAULT_BASELINE = os.path.join(
    _HERE, "..", "..", "compat", "openapi", "baseline", "weftdb.json"
)
_DEFAULT_CURRENT = os.path.join(
    _HERE, "..", "..", "compat", "openapi", "weftdb.json"
)

# "Non-trivial" default: any explicit default on a *new* query parameter is
# conservatively flagged for human review, because the spec alone cannot tell
# whether the default changes behaviour for clients that omit it.
# When the spec cannot tell us, we report it as a finding needing human review,
# which is a non-zero outcome.
_NON_TRIVIAL_DEFAULT_THRESHOLD = (
    "Any explicit default on a new query parameter is flagged as a finding "
    "needing human review (non-zero outcome), because the spec alone cannot "
    "tell whether the default changes behaviour for clients that omit it."
)


# ---------------------------------------------------------------------------
# Helpers
# ---------------------------------------------------------------------------


def _load_json(path: str) -> dict:
    """Load and return a JSON dict.  Exits 2 on failure."""
    try:
        with open(path, encoding="utf-8") as fh:
            return json.load(fh)
    except FileNotFoundError:
        print(f"ERROR: baseline not found: {path}", file=sys.stderr)
        sys.exit(2)
    except json.JSONDecodeError as exc:
        print(f"ERROR: unparseable JSON in {path}: {exc}", file=sys.stderr)
        sys.exit(2)
    except OSError as exc:
        print(f"ERROR: unreadable {path}: {exc}", file=sys.stderr)
        sys.exit(2)


def _normalise(spec: dict) -> dict:
    """Return a deep copy with version neutralised and keys sorted for
    structural comparison."""
    out = deepcopy(spec)
    info = out.get("info")
    if isinstance(info, dict):
        info["version"] = _VERSION_SENTINEL
    return out


def _has_nullable(schema: dict) -> bool:
    """Detect whether *schema* is nullable under either OpenAPI spelling."""
    # 3.0 style: ``nullable: true``
    if schema.get("nullable") is True:
        return True
    # 3.1 style: ``"type": ["t", "null"]``
    typ = schema.get("type")
    return bool(isinstance(typ, list) and "null" in typ)


def _type_shape(schema: dict) -> tuple:
    """A comparable representation of a schema's type shape."""
    t = schema.get("type")
    if isinstance(t, list):
        t = tuple(t)
    items = schema.get("items")
    if isinstance(items, dict):
        items = _type_shape(items)
    else:
        items = None
    ref = schema.get("$ref")
    addl = "additionalProperties" in schema
    return (t, items, ref, addl)


def _type_string(schema: dict) -> str:
    """Human-readable type summary, for error messages."""
    parts = []
    if "type" in schema:
        parts.append(str(schema["type"]))
    if "items" in schema:
        items = schema["items"]
        if isinstance(items, dict):
            parts.append(f"items:{items.get('type', '?')}")
    if "additionalProperties" in schema:
        parts.append("map")
    if schema.get("nullable"):
        parts.append("nullable")
    if "$ref" in schema:
        parts.append(schema["$ref"])
    return " ".join(parts) if parts else "object"


def _without_nullable(schema: dict) -> dict:
    """Return a copy of *schema* with the nullable aspect removed."""
    out = deepcopy(schema)
    out.pop("nullable", None)
    typ = out.get("type")
    if isinstance(typ, list) and "null" in typ:
        remaining = [t for t in typ if t != "null"]
        if len(remaining) == 1:
            out["type"] = remaining[0]
        elif len(remaining) > 1:
            out["type"] = remaining
    return out


def _param_key(p: dict) -> str:
    """Uniquely identify a parameter by (name, in)."""
    return f'{p["in"]}/{p["name"]}'


def _method_path(path: str, method: str) -> str:
    """Canonical location label."""
    return f"{path}.{method}"


# ---------------------------------------------------------------------------
# Resolving $ref in a spec
# ---------------------------------------------------------------------------


def _resolve_ref(spec: dict, ref: str) -> dict:
    """Follow a ``#/components/schemas/Name`` ref and return the schema dict."""
    if not ref.startswith("#/"):
        return {}
    parts = ref[2:].split("/")
    obj = spec
    for part in parts:
        if isinstance(obj, dict):
            obj = obj.get(part, {})
        else:
            return {}
    return obj if isinstance(obj, dict) else {}


# ---------------------------------------------------------------------------
# Schema / route consumers of a schema
# ---------------------------------------------------------------------------


def _schema_route_users(spec: dict, schema_name: str) -> list:
    """Return a sorted list of ``path.method`` strings whose operation
    references *schema_name* by $ref."""
    users = []
    for path, methods in spec.get("paths", {}).items():
        for method, op in _each_operation(methods):
            loc = _method_path(path, method)
            # Parameters
            for param in _collect_parameters(methods, op):
                _find_ref_in_value(param, schema_name, loc, users)
            # requestBody
            _find_ref_in_value(
                op.get("requestBody", {}), schema_name, loc, users
            )
            # Responses
            for code, resp in op.get("responses", {}).items():
                _find_ref_in_value(
                    resp, schema_name, f"{loc}.{code}", users
                )
    return sorted(set(users))


def _find_ref_in_value(obj, schema_name: str, loc: str, acc: list) -> None:
    """Recursively search for $ref referencing *schema_name*."""
    if isinstance(obj, dict):
        ref = obj.get("$ref", "")
        if ref.endswith(f"/{schema_name}"):
            acc.append(loc)
        for v in obj.values():
            _find_ref_in_value(v, schema_name, loc, acc)
    elif isinstance(obj, list):
        for item in obj:
            _find_ref_in_value(item, schema_name, loc, acc)


# ---------------------------------------------------------------------------
# Path / method helpers for the OpenAPI 3.1 structure
# ---------------------------------------------------------------------------


def _each_operation(methods: dict):
    """Yield ``(method, operation_dict)`` from a path's method dict."""
    for method, op in methods.items():
        if isinstance(op, dict):
            yield (method, op)


def _collect_parameters(methods: dict, op: dict) -> list:
    """Return path-level + operation-level parameters."""
    result = []
    # Path-level parameters (a list in the method dict)
    pl = methods.get("parameters")
    if isinstance(pl, list):
        result.extend(pl)
    # Operation-level parameters
    result.extend(op.get("parameters", []))
    return result


# ---------------------------------------------------------------------------
# Schema comparison
# ---------------------------------------------------------------------------


def _compare_schemas(
    base_schema: dict,
    curr_schema: dict,
    path_hint: str,
    spec: dict,
    baseline: dict,
    findings: list,
) -> None:
    """Compare two schema dicts and append findings."""
    # --- $ref ---
    base_ref = base_schema.get("$ref")
    curr_ref = curr_schema.get("$ref")
    if base_ref != curr_ref:
        if base_ref is None:
            findings.append(
                f"BREAKING removed-$ref {path_hint}: inline -> {curr_ref}"
            )
        elif curr_ref is None:
            findings.append(
                f"BREAKING removed-$ref {path_hint}: {base_ref} -> inline"
            )
        else:
            findings.append(
                f"BREAKING changed-$ref {path_hint}: {base_ref} -> {curr_ref}"
            )
        return
    if base_ref:
        # Both refer to the same $ref; resolve and compare the targets
        base_target = _resolve_ref(baseline, base_ref)
        curr_target = _resolve_ref(spec, base_ref)
        if base_target and curr_target:
            _compare_schemas(
                base_target, curr_target,
                f"{path_hint}(ref)", spec, baseline, findings
            )
        return

    # --- Nullable change (both directions) ---
    base_null = _has_nullable(base_schema)
    curr_null = _has_nullable(curr_schema)
    if base_null != curr_null:
        if not base_null and curr_null:
            findings.append(
                f"BREAKING always-present-nullable {path_hint}: "
                "not nullable -> nullable"
            )
        else:
            findings.append(
                f"BREAKING nullable-always-present {path_hint}: "
                "nullable -> not nullable"
            )

    # --- Type shape change ---
    base_clean = _without_nullable(base_schema)
    curr_clean = _without_nullable(curr_schema)
    if _type_shape(base_clean) != _type_shape(curr_clean):
        findings.append(
            f"BREAKING type-change {path_hint}: "
            f"{_type_string(base_schema)} -> {_type_string(curr_schema)}"
        )
        return

    # --- Constraint keys (tightening or loosening any is breaking) ---
    _CONSTRAINT_KEYS = [
        "minLength", "maxLength", "minimum", "maximum",
        "exclusiveMinimum", "exclusiveMaximum", "pattern",
        "enum", "maxItems", "minItems", "uniqueItems", "format",
    ]
    for key in _CONSTRAINT_KEYS:
        base_val = base_schema.get(key)
        curr_val = curr_schema.get(key)
        if base_val != curr_val:
            enum_reorder = (
                key == "enum"
                and isinstance(base_val, list)
                and isinstance(curr_val, list)
                and sorted(base_val) == sorted(curr_val)
            )
            if not enum_reorder:
                findings.append(
                f"BREAKING constraint-{key} {path_hint}: "
                f"{base_val!r} -> {curr_val!r}"
            )

    # --- Combinators (allOf / oneOf / anyOf) ---
    for kw in ("allOf", "oneOf", "anyOf"):
        base_members = base_schema.get(kw, [])
        curr_members = curr_schema.get(kw, [])
        if not isinstance(base_members, list) or not isinstance(curr_members, list):
            continue
        n = max(len(base_members), len(curr_members))
        for i in range(n):
            sp = f"{path_hint}.{kw}[{i}]"
            if i >= len(base_members):
                continue  # added member – not compared
            if i >= len(curr_members):
                findings.append(
                    f"BREAKING removed-{kw}-member {sp}: present -> absent"
                )
                continue
            base_m = base_members[i] if isinstance(base_members[i], dict) else {}
            curr_m = curr_members[i] if isinstance(curr_members[i], dict) else {}
            _compare_schemas(
                base_m, curr_m, sp, spec, baseline, findings,
            )

    # --- Properties ---
    base_props = base_schema.get("properties", {})
    curr_props = curr_schema.get("properties", {})
    base_required = base_schema.get("required", [])

    if not base_props and not curr_props:
        return  # scalar / leaf

    all_props = sorted(set(base_props) | set(curr_props))
    for prop in all_props:
        sp = f"{path_hint}.properties.{prop}"
        if prop not in base_props:
            # New property — check if required
            if (
                isinstance(curr_schema.get("required"), list)
                and prop in curr_schema["required"]
            ):
                findings.append(
                    f"BREAKING added-required-field {sp}: absent -> required"
                )
            continue
        if prop not in curr_props:
            findings.append(
                f"BREAKING removed-field {sp}: present -> absent"
            )
            continue

        # Requiredness change
        was_required = (
            isinstance(base_required, list) and prop in base_required
        )
        is_required = (
            isinstance(curr_schema.get("required"), list)
            and prop in curr_schema["required"]
        )
        if was_required and not is_required:
            findings.append(
                f"BREAKING required-optional {sp}: required -> optional"
            )
        elif not was_required and is_required:
            findings.append(
                f"BREAKING optional-required {sp}: optional -> required"
            )

        # Recurse into property schema
        _compare_schemas(
            base_props[prop], curr_props[prop], sp,
            spec, baseline, findings,
        )


# ---------------------------------------------------------------------------
# Operation comparison helpers
# ---------------------------------------------------------------------------


def _compare_content(
    base: dict,
    curr: dict,
    path_hint: str,
    spec: dict,
    baseline: dict,
    findings: list,
) -> None:
    """Compare ``content`` wrappers (OpenAPI MediaType)."""
    base_content = base.get("content", {})
    curr_content = curr.get("content", {})
    media_types = sorted(set(base_content) | set(curr_content))
    for mt in media_types:
        if mt not in base_content:
            continue
        if mt not in curr_content:
            findings.append(
                f"BREAKING removed-media-type {path_hint}.content.{mt}: "
                "present -> absent"
            )
            continue
        _compare_schemas(
            base_content[mt].get("schema", {}),
            curr_content[mt].get("schema", {}),
            f"{path_hint}.content.{mt}.schema",
            spec, baseline, findings,
        )


def _compare_operation(
    base_op: dict,
    curr_op: dict,
    methods: dict,
    curr_methods: dict,
    loc: str,
    spec: dict,
    baseline: dict,
    findings: list,
) -> None:
    """Compare two operation dicts for the same path+method."""
    # --- Parameters ---
    base_params_list = _collect_parameters(methods, base_op)
    curr_params_list = _collect_parameters(curr_methods, curr_op)
    base_params = {_param_key(p): p for p in base_params_list}
    curr_params = {_param_key(p): p for p in curr_params_list}

    for key in sorted(set(base_params) | set(curr_params)):
        if key not in base_params:
            p = curr_params[key]
            required = p.get("required", False)
            schema = p.get("schema", {})
            has_default = "default" in schema
            if required:
                findings.append(
                    f"BREAKING added-required-param "
                    f"{loc}.parameters.{key}: absent -> required"
                )
            elif has_default:
                findings.append(
                    f"BREAKING new-param-with-default "
                    f"{loc}.parameters.{key}: "
                    f"absent -> default={schema['default']}"
                )
            continue
        if key not in curr_params:
            findings.append(
                f"BREAKING removed-param {loc}.parameters.{key}: "
                "present -> absent"
            )
            continue

        bp = base_params[key]
        cp = curr_params[key]

        # Requiredness
        was_required = bp.get("required", False)
        is_required = cp.get("required", False)
        if was_required and not is_required:
            findings.append(
                f"BREAKING required-optional {loc}.parameters.{key}: "
                "required -> optional"
            )
        elif not was_required and is_required:
            findings.append(
                f"BREAKING optional-required {loc}.parameters.{key}: "
                "optional -> required"
            )

        # Default added to existing parameter
        base_schema = bp.get("schema", {})
        curr_schema = cp.get("schema", {})
        base_default = base_schema.get("default")
        curr_default = curr_schema.get("default")
        if base_default is None and curr_default is not None:
            findings.append(
                f"BREAKING added-default {loc}.parameters.{key}.schema: "
                f"absent -> default={curr_default!r}"
            )

        # Schema change
        _compare_schemas(
            base_schema, curr_schema,
            f"{loc}.parameters.{key}.schema",
            spec, baseline, findings,
        )

    # --- Request body ---
    _compare_request_body(base_op, curr_op, loc, spec, baseline, findings)

    # --- Responses ---
    _compare_responses(base_op, curr_op, loc, spec, baseline, findings)


def _compare_request_body(
    base_op: dict,
    curr_op: dict,
    path_hint: str,
    spec: dict,
    baseline: dict,
    findings: list,
) -> None:
    """Compare requestBody definitions."""
    base_rb = base_op.get("requestBody", {})
    curr_rb = curr_op.get("requestBody", {})
    if not base_rb and not curr_rb:
        return
    if not base_rb and curr_rb:
        # A new request body is additive when optional, but a *required*
        # request body forces clients to send data they did not need to
        # send before, which is the same class as adding a required query
        # parameter (docs/stability.md line 35: "adding a required query
        # parameter is breaking").
        if curr_rb.get("required", False):
            findings.append(
                f"BREAKING added-required-requestBody {path_hint}: "
                "absent -> required"
            )
        return
    if base_rb and not curr_rb:
        findings.append(
            f"BREAKING removed-requestBody {path_hint}: "
            "present -> absent"
        )
        return
    # --- Requiredness delta on existing requestBody ---
    # OpenAPI 3.x: ``required`` defaults to ``false`` when absent.
    was_required = base_rb.get("required", False)
    is_required = curr_rb.get("required", False)
    if was_required and not is_required:
        # Making a required request body optional is wire-relaxing, but a
        # client that was forced to send data before could now stop doing
        # so; this is the same class as ``required-optional`` on a field
        # (docs/stability.md:35 calls it breaking), so we flag it.
        findings.append(
            f"BREAKING required-optional-requestBody {path_hint}: "
            "required -> optional"
        )
    elif not was_required and is_required:
        findings.append(
            f"BREAKING optional-required-requestBody {path_hint}: "
            "optional -> required"
        )
    _compare_content(
        base_rb, curr_rb, f"{path_hint}.requestBody",
        spec, baseline, findings,
    )


def _compare_responses(
    base_op: dict,
    curr_op: dict,
    path_hint: str,
    spec: dict,
    baseline: dict,
    findings: list,
) -> None:
    """Compare response definitions by status code."""
    base_resp = base_op.get("responses", {})
    curr_resp = curr_op.get("responses", {})
    for code in sorted(set(base_resp) | set(curr_resp)):
        if code not in base_resp:
            # Adding a new response status code is additive (non-breaking),
            # analogous to adding a new route (docs/stability.md line 32).
            findings.append(
                f"INFO added-response-code {path_hint}.responses.{code}: "
                "absent -> present"
            )
            continue
        if code not in curr_resp:
            findings.append(
                f"BREAKING removed-response {path_hint}.responses.{code}: "
                "present -> absent"
            )
            continue
        _compare_content(
            base_resp[code], curr_resp[code],
            f"{path_hint}.responses.{code}",
            spec, baseline, findings,
        )


# ---------------------------------------------------------------------------
# Main comparison
# ---------------------------------------------------------------------------


def compare(baseline: dict, current: dict) -> list:
    """Compare two normalised specs and return a list of finding strings."""
    findings = []

    base_paths = baseline.get("paths", {})
    curr_paths = current.get("paths", {})

    if not isinstance(base_paths, dict):
        raise ValueError("baseline 'paths' is not an object")
    if not isinstance(curr_paths, dict):
        raise ValueError("current spec 'paths' is not an object")

    # --- Routes (paths + methods) ---
    all_paths = sorted(set(base_paths) | set(curr_paths))
    for path in all_paths:
        if path not in base_paths:
            continue  # new route – additive
        if path not in curr_paths:
            findings.append(
                f"BREAKING removed-route {path}: present -> absent"
            )
            continue

        base_methods = base_paths[path]
        curr_methods = curr_paths[path]

        # Collect the HTTP-method entries (skip path-level "parameters")
        base_ops = dict(_each_operation(base_methods))
        curr_ops = dict(_each_operation(curr_methods))

        for method in sorted(set(base_ops) | set(curr_ops)):
            loc = _method_path(path, method)
            if method not in base_ops:
                continue  # new method – additive
            if method not in curr_ops:
                findings.append(
                    f"BREAKING removed-route {loc}: present -> absent"
                )
                continue

            _compare_operation(
                base_ops[method], curr_ops[method],
                base_methods, curr_methods,
                loc, current, baseline, findings,
            )

    # --- Component schemas ---
    base_schemas = baseline.get("components", {}).get("schemas", {})
    curr_schemas = current.get("components", {}).get("schemas", {})

    if not isinstance(base_schemas, dict):
        base_schemas = {}
    if not isinstance(curr_schemas, dict):
        curr_schemas = {}

    for sname in sorted(set(base_schemas) | set(curr_schemas)):
        if sname not in base_schemas:
            continue
        if sname not in curr_schemas:
            users = _schema_route_users(current, sname)
            user_str = (
                f" (used by: {', '.join(users)})" if users else ""
            )
            findings.append(
                f"BREAKING removed-schema {sname}: present -> absent{user_str}"
            )
            continue

        _compare_schemas(
            base_schemas[sname], curr_schemas[sname],
            f"schemas.{sname}",
            current, baseline, findings,
        )

    return findings


# ---------------------------------------------------------------------------
# CLI
# ---------------------------------------------------------------------------


def main() -> int:
    parser = argparse.ArgumentParser(
        description="Check the live OpenAPI spec against the freeze baseline "
        "for breaking changes."
    )
    parser.add_argument(
        "--baseline", default=_DEFAULT_BASELINE,
        help=f"Path to the baseline spec (default: {_DEFAULT_BASELINE})",
    )
    parser.add_argument(
        "--current", default=_DEFAULT_CURRENT,
        help=f"Path to the current spec (default: {_DEFAULT_CURRENT})",
    )
    args = parser.parse_args()

    # Load
    try:
        baseline = _load_json(args.baseline)
    except SystemExit:
        return 2

    try:
        current = _load_json(args.current)
    except SystemExit:
        return 2

    # Validate shape
    base_paths = baseline.get("paths")
    curr_paths = current.get("paths")
    if not isinstance(base_paths, dict) or len(base_paths) == 0:
        print("ERROR: baseline 'paths' is missing or empty", file=sys.stderr)
        return 2
    if not isinstance(curr_paths, dict):
        print("ERROR: current spec 'paths' is missing", file=sys.stderr)
        return 2

    # Normalise
    base_norm = _normalise(baseline)
    curr_norm = _normalise(current)

    # Compare
    findings = compare(base_norm, curr_norm)

    # Report
    for f in findings:
        print(f)
    has_breaking = any(f.startswith("BREAKING") for f in findings)
    return 1 if has_breaking else 0


if __name__ == "__main__":
    try:
        sys.exit(main())
    except SystemExit:
        raise
    except Exception as exc:
        print(f"ERROR: {exc}", file=sys.stderr)
        sys.exit(2)
