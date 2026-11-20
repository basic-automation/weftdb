"""Tests for check-openapi-breaking.py.

The rule set is documented in docs/stability.md:27-40 and quoted in the
module docstring of the checker.
"""

import importlib.util
import json
import os
import tempfile
import unittest
from pathlib import Path

# ---------------------------------------------------------------------------
# Import the module under test by file path (same pattern as
# test_check_serial_order.py).
# ---------------------------------------------------------------------------
_MODULE_PATH = Path(__file__).with_name("check-openapi-breaking.py")
spec = importlib.util.spec_from_file_location(
    "check_openapi_breaking", str(_MODULE_PATH),
)
assert spec is not None and spec.loader is not None
cob = importlib.util.module_from_spec(spec)
spec.loader.exec_module(cob)


# ---------------------------------------------------------------------------
# Helper: build a minimal but structurally valid spec dict.
# ---------------------------------------------------------------------------

def _make_spec(
    paths: dict | None = None,
    schemas: dict | None = None,
    version: str = "0.3.4",
) -> dict:
    """Return a minimal OpenAPI spec dict with the given paths and schemas."""
    out = {
        "openapi": "3.1.0",
        "info": {
            "title": "test-spec",
            "version": version,
            "description": "test fixture",
        },
        "paths": paths or {},
        "components": {
            "schemas": schemas or {},
        },
    }
    return out


def _make_op(
    parameters: list | None = None,
    request_body: dict | None = None,
    responses: dict | None = None,
) -> dict:
    """Return a minimal operation dict."""
    op: dict = {
        "operationId": "test_op",
        "summary": "test",
    }
    if parameters is not None:
        op["parameters"] = parameters
    if request_body is not None:
        op["requestBody"] = request_body
    if responses is not None:
        op["responses"] = responses
    else:
        op["responses"] = {
            "200": {
                "description": "OK",
            }
        }
    return op


def _json_body(schema: dict) -> dict:
    """Wrap *schema* in a standard requestBody/response JSON content."""
    return {
        "description": "test body",
        "content": {
            "application/json": {
                "schema": schema,
            }
        },
    }


def _prop(name: str, typ: str, required: bool = False) -> dict:
    """Return a property entry plus an entry for the schema's required list.
    Returns ``(prop_dict, required_name_or_none)``."""
    return {"type": typ}


def _obj_schema(
    properties: dict[str, dict],
    required: list[str] | None = None,
) -> dict:
    """Build an object schema with the given properties."""
    return {
        "type": "object",
        "properties": properties,
        "required": required or [],
    }


# ===================================================================
# Smoke test: real committed spec vs baseline (both are the same file)
# ===================================================================

class SmokeTest(unittest.TestCase):
    """One smoke test that the committed baseline and spec are identical."""

    def test_committed_is_identical_to_baseline(self) -> None:
        """The committed spec and the baseline copy must be byte-identical
        right now (they were created from the same commit)."""
        base_path = cob._DEFAULT_BASELINE
        curr_path = cob._DEFAULT_CURRENT
        with open(base_path, "rb") as f:
            base_bytes = f.read()
        with open(curr_path, "rb") as f:
            curr_bytes = f.read()
        self.assertEqual(
            base_bytes, curr_bytes,
            "baseline and committed spec differ; was the baseline "
            "regenerated without updating the copy?",
        )


# ===================================================================
# Route-level changes
# ===================================================================

class AddedRouteTest(unittest.TestCase):
    """Adding a route is additive (non-breaking)."""

    def test_added_route_passes(self) -> None:
        base = _make_spec(paths={})
        curr = _make_spec(paths={
            "/api/v1/new": {
                "get": _make_op(),
            },
        })
        base_norm = cob._normalise(base)
        curr_norm = cob._normalise(curr)
        findings = cob.compare(base_norm, curr_norm)
        self.assertEqual(findings, [])


class RemovedRouteTest(unittest.TestCase):
    """Removing or renaming a route is breaking."""

    def test_removed_route_fails(self) -> None:
        base = _make_spec(paths={
            "/api/v1/old": {
                "get": _make_op(),
            },
        })
        curr = _make_spec(paths={})
        base_norm = cob._normalise(base)
        curr_norm = cob._normalise(curr)
        findings = cob.compare(base_norm, curr_norm)
        self.assertTrue(
            any("BREAKING" in f and "removed-route" in f for f in findings),
            f"expected a breaking finding, got {findings}",
        )
        self.assertIn("/api/v1/old", str(findings))


# ===================================================================
# Field-level changes
# ===================================================================

class AddedOptionalFieldTest(unittest.TestCase):
    """Adding an optional field to a request body or response is non-breaking."""

    def test_added_optional_body_field_passes(self) -> None:
        base = _make_spec(paths={
            "/api/v1/test": {
                "post": _make_op(
                    request_body=_json_body(
                        _obj_schema({"name": {"type": "string"}}, ["name"])
                    ),
                ),
            },
        })
        curr = _make_spec(paths={
            "/api/v1/test": {
                "post": _make_op(
                    request_body=_json_body(
                        _obj_schema(
                            {
                                "name": {"type": "string"},
                                "label": {"type": "string"},
                            },
                            ["name"],
                        )
                    ),
                ),
            },
        })
        base_norm = cob._normalise(base)
        curr_norm = cob._normalise(curr)
        findings = cob.compare(base_norm, curr_norm)
        self.assertEqual(findings, [])


class RemovedFieldTest(unittest.TestCase):
    """Removing a field is breaking."""

    def test_removed_body_field_fails(self) -> None:
        base = _make_spec(paths={
            "/api/v1/test": {
                "post": _make_op(
                    request_body=_json_body(
                        _obj_schema(
                            {
                                "name": {"type": "string"},
                                "label": {"type": "string"},
                            },
                            ["name"],
                        )
                    ),
                ),
            },
        })
        curr = _make_spec(paths={
            "/api/v1/test": {
                "post": _make_op(
                    request_body=_json_body(
                        _obj_schema({"name": {"type": "string"}}, ["name"])
                    ),
                ),
            },
        })
        base_norm = cob._normalise(base)
        curr_norm = cob._normalise(curr)
        findings = cob.compare(base_norm, curr_norm)
        self.assertTrue(
            any("BREAKING" in f and "removed-field" in f for f in findings),
            f"expected a removed-field finding, got {findings}",
        )


class RequiredToOptionalTest(unittest.TestCase):
    """Required -> optional is breaking."""

    def test_required_to_optional_fails(self) -> None:
        base = _make_spec(paths={
            "/api/v1/test": {
                "post": _make_op(
                    request_body=_json_body(
                        _obj_schema(
                            {"name": {"type": "string"}}, ["name"]
                        )
                    ),
                ),
            },
        })
        curr = _make_spec(paths={
            "/api/v1/test": {
                "post": _make_op(
                    request_body=_json_body(
                        _obj_schema(
                            {"name": {"type": "string"}}, []
                        )
                    ),
                ),
            },
        })
        base_norm = cob._normalise(base)
        curr_norm = cob._normalise(curr)
        findings = cob.compare(base_norm, curr_norm)
        self.assertTrue(
            any("BREAKING" in f and "required-optional" in f for f in findings),
            f"expected a required-optional finding, got {findings}",
        )


class TypeChangeTest(unittest.TestCase):
    """Type constraint tightened or loosened (type change) is breaking."""

    def test_type_change_fails(self) -> None:
        base = _make_spec(paths={
            "/api/v1/test": {
                "get": _make_op(
                    parameters=[
                        {
                            "in": "query",
                            "name": "limit",
                            "required": False,
                            "schema": {"type": "integer"},
                        }
                    ],
                ),
            },
        })
        curr = _make_spec(paths={
            "/api/v1/test": {
                "get": _make_op(
                    parameters=[
                        {
                            "in": "query",
                            "name": "limit",
                            "required": False,
                            "schema": {"type": "string"},
                        }
                    ],
                ),
            },
        })
        base_norm = cob._normalise(base)
        curr_norm = cob._normalise(curr)
        findings = cob.compare(base_norm, curr_norm)
        self.assertTrue(
            any("BREAKING" in f and "type-change" in f for f in findings),
            f"expected a type-change finding, got {findings}",
        )


# ===================================================================
# Nullable changes (both OpenAPI spellings)
# ===================================================================

class NullableToAlwaysPresentTest(unittest.TestCase):
    """Nullable -> always-present response field is also breaking (tightening)."""
    """Nullable -> always-present response field is also breaking (tightening)."""

    def _non_nullify_via_nullable_false(self) -> None:
        """3.0 spelling:``nullable: true`` -> ``nullable: false``."""
        base = _make_spec(paths={
            "/api/v1/test": {
                "get": _make_op(
                    responses={
                        "200": _json_body(
                            _obj_schema(
                                {
                                    "total": {
                                        "type": "integer",
                                        "nullable": True,
                                    }
                                },
                                ["total"],
                            )
                        ),
                    },
                ),
            },
        })
        curr = _make_spec(paths={
            "/api/v1/test": {
                "get": _make_op(
                    responses={
                        "200": _json_body(
                            _obj_schema(
                                {"total": {"type": "integer"}}, ["total"]
                            )
                        ),
                    },
                ),
            },
        })
        base_norm = cob._normalise(base)
        curr_norm = cob._normalise(curr)
        findings = cob.compare(base_norm, curr_norm)
        self.assertTrue(
            any("BREAKING" in f and "nullable" in f for f in findings),
            f"expected a nullable finding, got {findings}",
        )

    def test_nullable_true_to_false_fails(self) -> None:
        self._non_nullify_via_nullable_false()

    def test_nullable_type_array_to_single_fails(self) -> None:
        """3.1 spelling: ``type: ["integer", "null"]`` -> ``type: "integer"``."""
        base = _make_spec(paths={
            "/api/v1/test": {
                "get": _make_op(
                    responses={
                        "200": _json_body(
                            _obj_schema(
                                {
                                    "total": {
                                        "type": ["integer", "null"],
                                    }
                                },
                                ["total"],
                            )
                        ),
                    },
                ),
            },
        })
        curr = _make_spec(paths={
            "/api/v1/test": {
                "get": _make_op(
                    responses={
                        "200": _json_body(
                            _obj_schema(
                                {"total": {"type": "integer"}}, ["total"]
                            )
                        ),
                    },
                ),
            },
        })
        base_norm = cob._normalise(base)
        curr_norm = cob._normalise(curr)
        findings = cob.compare(base_norm, curr_norm)
        self.assertTrue(
            any("BREAKING" in f and "nullable" in f for f in findings),
            f"expected a nullable finding, got {findings}",
        )

    def test_delete_nullable_key_fails(self) -> None:
        """Deleting ``"nullable": true`` key (becomes absent) is breaking."""
        base = _make_spec(paths={
            "/api/v1/test": {
                "get": _make_op(
                    responses={
                        "200": _json_body(
                            _obj_schema(
                                {
                                    "total": {
                                        "type": "integer",
                                        "nullable": True,
                                    }
                                },
                                ["total"],
                            )
                        ),
                    },
                ),
            },
        })
        curr = _make_spec(paths={
            "/api/v1/test": {
                "get": _make_op(
                    responses={
                        "200": _json_body(
                            _obj_schema(
                                {"total": {"type": "integer"}}, ["total"]
                            )
                        ),
                    },
                ),
            },
        })
        base_norm = cob._normalise(base)
        curr_norm = cob._normalise(curr)
        findings = cob.compare(base_norm, curr_norm)
        self.assertTrue(
            any("BREAKING" in f and "nullable" in f for f in findings),
            f"expected a nullable finding, got {findings}",
        )


class AlwaysPresentToNullableTest(unittest.TestCase):
    """Always-present -> nullable response field is breaking."""

    def _nullify_via_nullable_true(self) -> None:
        """3.0 spelling: ``nullable: true``."""
        base = _make_spec(paths={
            "/api/v1/test": {
                "get": _make_op(
                    responses={
                        "200": _json_body(
                            _obj_schema(
                                {"total": {"type": "integer"}}, ["total"]
                            )
                        ),
                    },
                ),
            },
        })
        curr = _make_spec(paths={
            "/api/v1/test": {
                "get": _make_op(
                    responses={
                        "200": _json_body(
                            _obj_schema(
                                {
                                    "total": {
                                        "type": "integer",
                                        "nullable": True,
                                    }
                                },
                                ["total"],
                            )
                        ),
                    },
                ),
            },
        })
        base_norm = cob._normalise(base)
        curr_norm = cob._normalise(curr)
        findings = cob.compare(base_norm, curr_norm)
        self.assertTrue(
            any("BREAKING" in f and "nullable" in f for f in findings),
            f"expected a nullable finding, got {findings}",
        )

    def test_nullable_true_spelling_fails(self) -> None:
        self._nullify_via_nullable_true()

    def test_nullable_type_array_spelling_fails(self) -> None:
        """3.1 spelling: ``type: ["integer", "null"]``."""
        base = _make_spec(paths={
            "/api/v1/test": {
                "get": _make_op(
                    responses={
                        "200": _json_body(
                            _obj_schema(
                                {"total": {"type": "integer"}}, ["total"]
                            )
                        ),
                    },
                ),
            },
        })
        curr = _make_spec(paths={
            "/api/v1/test": {
                "get": _make_op(
                    responses={
                        "200": _json_body(
                            _obj_schema(
                                {
                                    "total": {
                                        "type": ["integer", "null"],
                                    }
                                },
                                ["total"],
                            )
                        ),
                    },
                ),
            },
        })
        base_norm = cob._normalise(base)
        curr_norm = cob._normalise(curr)
        findings = cob.compare(base_norm, curr_norm)
        self.assertTrue(
            any("BREAKING" in f and "nullable" in f for f in findings),
            f"expected a nullable finding, got {findings}",
        )


# ===================================================================
# Query parameter with default
# ===================================================================

class AddedParameterWithDefaultTest(unittest.TestCase):
    """Adding a query parameter with a non-trivial default -> finding."""

    def test_added_default_param_yields_finding(self) -> None:
        base = _make_spec(paths={
            "/api/v1/test": {
                "get": _make_op(parameters=[]),
            },
        })
        curr = _make_spec(paths={
            "/api/v1/test": {
                "get": _make_op(
                    parameters=[
                        {
                            "in": "query",
                            "name": "format",
                            "required": False,
                            "schema": {
                                "type": "string",
                                "default": "json",
                            },
                        }
                    ],
                ),
            },
        })
        base_norm = cob._normalise(base)
        curr_norm = cob._normalise(curr)
        findings = cob.compare(base_norm, curr_norm)
        self.assertTrue(
            any("BREAKING" in f and "new-param-with-default" in f for f in findings),
            f"expected a new-param-with-default finding, got {findings}",
        )

    def test_added_optional_param_no_default_passes(self) -> None:
        base = _make_spec(paths={
            "/api/v1/test": {
                "get": _make_op(parameters=[]),
            },
        })
        curr = _make_spec(paths={
            "/api/v1/test": {
                "get": _make_op(
                    parameters=[
                        {
                            "in": "query",
                            "name": "format",
                            "required": False,
                            "schema": {"type": "string"},
                        }
                    ],
                ),
            },
        })
        base_norm = cob._normalise(base)
        curr_norm = cob._normalise(curr)
        findings = cob.compare(base_norm, curr_norm)
        self.assertEqual(findings, [])


# ===================================================================
# Format / ordering changes (should be clean)
# ===================================================================

class FormatOrderPassesTest(unittest.TestCase):
    """Format/order-only passes (normalised away)."""

    def test_version_only_change_passes(self) -> None:
        base = _make_spec(
            paths={"/api/v1/ping": {"get": _make_op()}},
            version="0.3.4",
        )
        curr = _make_spec(
            paths={"/api/v1/ping": {"get": _make_op()}},
            version="0.5.0",
        )
        base_norm = cob._normalise(base)
        curr_norm = cob._normalise(curr)
        findings = cob.compare(base_norm, curr_norm)
        self.assertEqual(findings, [])

    def test_key_order_passes(self) -> None:
        """Key ordering differences should not count as changes (normalised
        through the same JSON parsing)."""
        base = _make_spec(paths={
            "/api/v1/test": {
                "get": _make_op(
                    parameters=[
                        {
                            "in": "query",
                            "name": "a",
                            "required": True,
                            "schema": {"type": "string"},
                        },
                        {
                            "in": "query",
                            "name": "b",
                            "required": False,
                            "schema": {"type": "integer"},
                        },
                    ],
                ),
            },
        })
        # Same data but with different param order
        curr = _make_spec(paths={
            "/api/v1/test": {
                "get": _make_op(
                    parameters=[
                        {
                            "in": "query",
                            "name": "b",
                            "required": False,
                            "schema": {"type": "integer"},
                        },
                        {
                            "in": "query",
                            "name": "a",
                            "required": True,
                            "schema": {"type": "string"},
                        },
                    ],
                ),
            },
        })
        base_norm = cob._normalise(base)
        curr_norm = cob._normalise(curr)
        findings = cob.compare(base_norm, curr_norm)
        self.assertEqual(findings, [])


# ===================================================================
# Missing baseline
# ===================================================================

class MissingBaselineTest(unittest.TestCase):
    """Missing baseline -> exit 2."""

    def test_missing_baseline_exits_2(self) -> None:
        """Calling main() with a non-existent baseline must exit 2."""
        with tempfile.NamedTemporaryFile(
            suffix=".json", mode="w", delete=False,
        ) as f:
            json.dump({"openapi": "3.1.0", "info": {"version": "1"}, "paths": {}}, f)
            current_path = f.name
        # Directly test _load_json with a non-existent path.
        nonexistent = current_path + ".does_not_exist"
        with self.assertRaises(SystemExit) as cm:
            cob._load_json(nonexistent)
        self.assertEqual(cm.exception.code, 2)
        os.unlink(current_path)


# ===================================================================
# Additional fine-grained checks
# ===================================================================

class AdditionalParamChangesTest(unittest.TestCase):
    """Extra parameter-level guards."""

    def test_added_required_param_fails(self) -> None:
        base = _make_spec(paths={
            "/api/v1/test": {
                "get": _make_op(parameters=[]),
            },
        })
        curr = _make_spec(paths={
            "/api/v1/test": {
                "get": _make_op(
                    parameters=[
                        {
                            "in": "query",
                            "name": "required_param",
                            "required": True,
                            "schema": {"type": "string"},
                        }
                    ],
                ),
            },
        })
        base_norm = cob._normalise(base)
        curr_norm = cob._normalise(curr)
        findings = cob.compare(base_norm, curr_norm)
        self.assertTrue(
            any("BREAKING" in f and "added-required-param" in f for f in findings),
            f"expected added-required-param finding, got {findings}",
        )

    def test_removed_param_fails(self) -> None:
        base = _make_spec(paths={
            "/api/v1/test": {
                "get": _make_op(
                    parameters=[
                        {
                            "in": "query",
                            "name": "old_param",
                            "required": False,
                            "schema": {"type": "string"},
                        }
                    ],
                ),
            },
        })
        curr = _make_spec(paths={
            "/api/v1/test": {
                "get": _make_op(parameters=[]),
            },
        })
        base_norm = cob._normalise(base)
        curr_norm = cob._normalise(curr)
        findings = cob.compare(base_norm, curr_norm)
        self.assertTrue(
            any("BREAKING" in f and "removed-param" in f for f in findings),
            f"expected removed-param finding, got {findings}",
        )


if __name__ == "__main__":
    unittest.main()
