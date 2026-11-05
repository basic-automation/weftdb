"""The WeftDB HTTP client.

One :class:`Client` per server. Every method is one request to one route of
``weft-server``'s HTTP API (``docs/reference.md``, "HTTP API"), named for what it does
rather than for its path, plus two iterators that follow the stored-read cursors.

Two value conventions follow the server's own numeric boundary:

* the **compute** endpoints (``interpolate``, ``downsample``) speak ``f64`` and RFC 3339
  timestamps, so points go in as ``(datetime | str, float)`` pairs or dicts;
* the **storage** endpoints are lossless, so stored timestamps are integers in the
  aspect's declared unit and values travel as decimal *text*. Pass a
  :class:`decimal.Decimal`, a ``str`` or an ``int``; a ``float`` is refused, because its
  binary expansion is not the number you typed.

Arrow and Parquet answers come back as a ``pyarrow.Table`` (install the ``arrow`` extra),
or as the raw bytes with ``raw=True``.
"""

from __future__ import annotations

import hashlib
import json
import time
import urllib.error
import urllib.parse
import urllib.request
from datetime import datetime, timezone
from decimal import Decimal
from typing import Any, Dict, Iterable, Iterator, List, Mapping, Optional, Sequence, Tuple, Union

from . import __version__
from .errors import IntegrityError, Overloaded, WeftError, error_class

Timestamp = Union[datetime, str, int, float]
"""A compute-endpoint instant: an aware ``datetime``, an RFC 3339 string, or seconds since the epoch."""

StoredValue = Union[Decimal, str, int, None]
"""A storage-endpoint value: decimal text in any lossless form, or ``None`` for a null."""

FORMATS = ("json", "csv", "arrow", "parquet")

_ARROW_STREAM = "application/vnd.apache.arrow.stream"


def _rfc3339(instant: Timestamp) -> str:
    if isinstance(instant, str):
        return instant
    if isinstance(instant, datetime):
        if instant.tzinfo is None:
            raise ValueError(f"{instant!r} has no timezone; the compute endpoints take UTC instants, so attach one (tzinfo=timezone.utc)")
        return instant.astimezone(timezone.utc).isoformat().replace("+00:00", "Z")
    if isinstance(instant, (int, float)) and not isinstance(instant, bool):
        return datetime.fromtimestamp(instant, tz=timezone.utc).isoformat().replace("+00:00", "Z")
    raise TypeError(f"{instant!r} is not a timestamp (datetime, RFC 3339 string, or epoch seconds)")


def _compute_points(points: Iterable[Any]) -> List[Dict[str, Any]]:
    """``(timestamp, value)`` pairs or ``{"timestamp", "value"}`` dicts, as the compute body."""
    out = []
    for point in points:
        if isinstance(point, Mapping):
            timestamp, value = point["timestamp"], point["value"]
        else:
            timestamp, value = point
        out.append({"timestamp": _rfc3339(timestamp), "value": float(value)})
    return out


def _decimal_text(value: StoredValue) -> Optional[str]:
    if value is None:
        return None
    if isinstance(value, bool):
        raise TypeError("a bool is not a stored value")
    if isinstance(value, float):
        raise TypeError(f"{value!r} is a float; the storage endpoints are lossless, so pass Decimal or str (Decimal({str(value)!r}) if that is the number you mean)")
    if isinstance(value, Decimal):
        if not value.is_finite():
            raise ValueError(f"{value!r} is not finite")
        return format(value, "f")
    if isinstance(value, (int, str)):
        return str(value)
    raise TypeError(f"{value!r} is not a stored value (Decimal, str, int or None)")


def _stored_points(points: Iterable[Any]) -> List[Dict[str, Any]]:
    """``(timestamp, value)`` pairs or dicts, as the lossless ingest body."""
    out = []
    for point in points:
        if isinstance(point, Mapping):
            timestamp, value = point["timestamp"], point.get("value")
        else:
            timestamp, value = point
        if isinstance(timestamp, bool) or not isinstance(timestamp, int):
            raise TypeError(f"stored timestamps are integers in the aspect's declared unit, not {timestamp!r}")
        out.append({"timestamp": timestamp, "value": _decimal_text(value)})
    return out


def _agg_param(aggregations: Union[str, Sequence[str], None]) -> Optional[str]:
    if aggregations is None or isinstance(aggregations, str):
        return aggregations
    return ",".join(aggregations)


def _require_pyarrow():
    try:
        import pyarrow  # noqa: F401
    except ImportError as missing:  # pragma: no cover - exercised only without pyarrow
        raise ImportError("Arrow and Parquet answers decode with pyarrow: pip install 'weftdb[arrow]', or pass raw=True for the bytes") from missing
    return pyarrow


def decode_arrow(data: bytes):
    """An Arrow IPC stream as a ``pyarrow.Table``."""
    pa = _require_pyarrow()
    import pyarrow.ipc

    return pyarrow.ipc.open_stream(pa.BufferReader(data)).read_all()


def decode_parquet(data: bytes):
    """A Parquet file as a ``pyarrow.Table``."""
    pa = _require_pyarrow()
    import pyarrow.parquet

    return pyarrow.parquet.read_table(pa.BufferReader(data))


class Response:
    """A successful answer: status, headers and body bytes."""

    def __init__(self, status: int, headers: Mapping[str, str], body: bytes) -> None:
        self.status = status
        self.headers = {key.lower(): value for key, value in headers.items()}
        self.body = body

    def json(self) -> Any:
        return json.loads(self.body)

    def text(self) -> str:
        return self.body.decode("utf-8")


class Client:
    """A connection to one ``weft-server``.

    :param base_url: the server's origin, e.g. ``http://127.0.0.1:8080``.
    :param api_key: a ``weft_…`` key from ``weft-server keygen``, sent as a Bearer token.
        Never put it in the URL: the server refuses a key in a query string.
    :param timeout: seconds per request.
    :param max_retries: how many times a read refused ``503 overloaded`` is retried, after
        the server's ``Retry-After``. Writes are never retried by the client: send them
        with an ``idempotency_key`` and retry yourself if you need to.
    """

    def __init__(self, base_url: str = "http://127.0.0.1:8080", *, api_key: Optional[str] = None, timeout: float = 30.0, max_retries: int = 2, user_agent: Optional[str] = None) -> None:
        parsed = urllib.parse.urlsplit(base_url)
        if parsed.scheme not in ("http", "https") or not parsed.netloc:
            raise ValueError(f"{base_url!r} is not an http(s) origin")
        if parsed.query:
            raise ValueError("the base URL carries no query; credentials go in api_key, never the URL")
        self.base_url = base_url.rstrip("/")
        self.api_key = api_key
        self.timeout = timeout
        self.max_retries = max_retries
        self.user_agent = user_agent or f"weftdb-python/{__version__}"

    def __repr__(self) -> str:
        return f"Client({self.base_url!r}, api_key={'set' if self.api_key else None})"

    # ── transport ────────────────────────────────────────────────────────────

    def request(self, method: str, path: str, *, params: Optional[Mapping[str, Any]] = None, body: Optional[bytes] = None, content_type: Optional[str] = None, headers: Optional[Mapping[str, str]] = None, accept: Sequence[int] = ()) -> Response:
        """Send one request and return its answer, or raise the :class:`WeftError` its envelope names.

        A refusal whose status is in ``accept`` is returned as a :class:`Response` instead.
        """
        url = self.base_url + path
        query = {key: value for key, value in (params or {}).items() if value is not None}
        if query:
            url += "?" + urllib.parse.urlencode({key: str(value).lower() if isinstance(value, bool) else value for key, value in query.items()})
        sent = {"Accept": "*/*", "User-Agent": self.user_agent}
        if self.api_key:
            sent["Authorization"] = f"Bearer {self.api_key}"
        if content_type:
            sent["Content-Type"] = content_type
        sent.update(headers or {})
        attempt = 0
        while True:
            request = urllib.request.Request(url, data=body, method=method, headers=sent)
            try:
                with urllib.request.urlopen(request, timeout=self.timeout) as answer:
                    return Response(answer.status, dict(answer.headers.items()), answer.read())
            except urllib.error.HTTPError as refusal:
                with refusal:
                    if refusal.code in accept:
                        return Response(refusal.code, dict(refusal.headers.items()), refusal.read())
                    error = self._error_from(refusal)
                if isinstance(error, Overloaded) and method == "GET" and attempt < self.max_retries:
                    attempt += 1
                    time.sleep(min(1.0 if error.retry_after is None else error.retry_after, 30.0))
                    continue
                raise error from None
            except urllib.error.URLError as unreachable:
                raise WeftError(f"cannot reach {self.base_url}: {unreachable.reason}") from unreachable

    @staticmethod
    def _error_from(refusal: urllib.error.HTTPError) -> WeftError:
        raw = refusal.read()
        try:
            envelope = json.loads(raw)
        except ValueError:
            envelope = None
        if not isinstance(envelope, dict):
            envelope = {"error": raw.decode("utf-8", "replace").strip() or refusal.reason}
        retry_after = refusal.headers.get("Retry-After")
        try:
            retry = float(retry_after) if retry_after is not None else None
        except ValueError:
            retry = None
        code = envelope.get("code")
        cls = error_class(refusal.code, code)
        return cls(str(envelope.get("error", refusal.reason)), status=refusal.code, code=code, details=envelope.get("details"), limit=envelope.get("limit"), committed=envelope.get("committed"), request_id=envelope.get("request_id") or refusal.headers.get("x-request-id"), retry_after=retry)

    def _get(self, path: str, **params: Any) -> Response:
        return self.request("GET", path, params=params)

    def _post_json(self, path: str, payload: Any, *, params: Optional[Mapping[str, Any]] = None, headers: Optional[Mapping[str, str]] = None) -> Response:
        return self.request("POST", path, params=params, body=json.dumps(payload).encode("utf-8"), content_type="application/json", headers=headers)

    @staticmethod
    def _aspect(aspect: str) -> str:
        return urllib.parse.quote(aspect, safe="")

    @staticmethod
    def _decode(response: Response, fmt: str, raw: bool) -> Any:
        if fmt == "json":
            return response.json()
        if fmt == "csv":
            return response.text()
        if raw:
            return response.body
        return decode_arrow(response.body) if fmt == "arrow" else decode_parquet(response.body)

    @staticmethod
    def _check_format(fmt: str, allowed: Sequence[str] = FORMATS) -> str:
        if fmt not in allowed:
            raise ValueError(f"format {fmt!r} is not one of {', '.join(allowed)}")
        return fmt

    # ── service ──────────────────────────────────────────────────────────────

    def health(self) -> Dict[str, Any]:
        """``GET /health``: the process is up."""
        return self._get("/health").json()

    def ready(self) -> Dict[str, Any]:
        """``GET /ready``: whether the server can serve, and its store's identity and health.

        A ``503`` here (a store this build cannot open) is still the readiness document, with
        ``ready: false``, so it is returned rather than raised.
        """
        return self.request("GET", "/ready", accept=(503,)).json()

    def metrics(self) -> str:
        """``GET /metrics``: the Prometheus text exposition (``metrics`` scope)."""
        return self._get("/metrics").text()

    def profile(self) -> Dict[str, Any]:
        """``GET /debug/profile/current``: live p50/p95/p99 per instrumented path (``metrics`` scope)."""
        return self._get("/debug/profile/current").json()

    def bench_runs(self) -> List[Dict[str, Any]]:
        """``GET /bench/runs``: the benchmark runs the server holds, newest first (experimental)."""
        return self._get("/bench/runs").json()["runs"]

    def bench_run(self, run_id: str, *, raw: bool = False) -> Any:
        """``GET /bench/runs/{id}``: one run's manifest (experimental).

        The id is the SHA-256 of the manifest's bytes, so the answer is checked against it:
        bytes that do not hash to ``run_id`` raise :class:`IntegrityError` instead of being
        returned as that run.
        """
        response = self._get(f"/bench/runs/{urllib.parse.quote(run_id, safe='')}")
        actual = hashlib.sha256(response.body).hexdigest()
        if actual != run_id:
            raise IntegrityError(f"the server answered run {run_id} with bytes that hash to {actual}")
        return response.body if raw else response.json()

    # ── compute ──────────────────────────────────────────────────────────────

    def interpolate(self, points: Iterable[Any], *, spline: Union[str, Mapping[str, Any]] = "cubic", resolution: str = "seconds", start: Optional[Timestamp] = None, end: Optional[Timestamp] = None, format: str = "json", raw: bool = False) -> Any:
        """``POST /api/v1/interpolate[/csv|/arrow|/parquet]``: reconstruct ``points`` on a regular grid.

        ``spline`` is ``linear``, ``quadratic``, ``cubic``, or ``{"polynomial": {"degree": n}}``.
        Every output point carries a ``kind``: ``raw``, ``interpolated`` or ``extrapolated``.
        """
        fmt = self._check_format(format)
        payload: Dict[str, Any] = {"spline": spline, "resolution": resolution, "points": _compute_points(points)}
        if start is not None:
            payload["start"] = _rfc3339(start)
        if end is not None:
            payload["end"] = _rfc3339(end)
        suffix = "" if fmt == "json" else f"/{fmt}"
        return self._decode(self._post_json(f"/api/v1/interpolate{suffix}", payload), fmt, raw)

    def interpolate_point(self, points: Iterable[Any], instant: Timestamp, *, spline: Union[str, Mapping[str, Any]] = "cubic") -> Dict[str, Any]:
        """``POST /api/v1/interpolate/point``: the reconstructed signal at one instant."""
        return self._post_json("/api/v1/interpolate/point", {"spline": spline, "instant": _rfc3339(instant), "points": _compute_points(points)}).json()

    def downsample(self, points: Iterable[Any], *, aggregations: Union[str, Sequence[str]] = ("avg",), resolution: str = "seconds", start: Optional[Timestamp] = None, end: Optional[Timestamp] = None, width: Optional[str] = None, fill: Optional[str] = None, format: str = "json", raw: bool = False) -> Any:
        """``POST /api/v1/downsample[/csv|/arrow|/parquet]``: reduce ``points`` into epoch-aligned buckets."""
        fmt = self._check_format(format)
        names = [aggregations] if isinstance(aggregations, str) else list(aggregations)
        payload: Dict[str, Any] = {"resolution": resolution, "aggregations": names, "points": _compute_points(points)}
        for key, value in (("start", start), ("end", end)):
            if value is not None:
                payload[key] = _rfc3339(value)
        if width is not None:
            payload["width"] = width
        if fill is not None:
            payload["fill"] = fill
        suffix = "" if fmt == "json" else f"/{fmt}"
        return self._decode(self._post_json(f"/api/v1/downsample{suffix}", payload), fmt, raw)

    def interpolate_ilp(self, body: Union[str, bytes], *, field: str, precision: Optional[str] = None, spline: Optional[str] = None, resolution: Optional[str] = None, format: str = "json", raw: bool = False) -> Any:
        """``POST /api/v1/interpolate/ilp[/…]``: interpolate one field of an InfluxDB Line Protocol body."""
        fmt = self._check_format(format)
        suffix = "" if fmt == "json" else f"/{fmt}"
        data = body.encode("utf-8") if isinstance(body, str) else body
        response = self.request("POST", f"/api/v1/interpolate/ilp{suffix}", params={"field": field, "precision": precision, "spline": spline, "resolution": resolution}, body=data, content_type="text/plain")
        return self._decode(response, fmt, raw)

    def downsample_ilp(self, body: Union[str, bytes], *, field: str, precision: Optional[str] = None, resolution: Optional[str] = None, aggregations: Union[str, Sequence[str], None] = None, fill: Optional[str] = None, format: str = "json", raw: bool = False) -> Any:
        """``POST /api/v1/downsample/ilp[/…]``: downsample one field of an InfluxDB Line Protocol body."""
        fmt = self._check_format(format)
        suffix = "" if fmt == "json" else f"/{fmt}"
        data = body.encode("utf-8") if isinstance(body, str) else body
        response = self.request("POST", f"/api/v1/downsample/ilp{suffix}", params={"field": field, "precision": precision, "resolution": resolution, "agg": _agg_param(aggregations), "fill": fill}, body=data, content_type="text/plain")
        return self._decode(response, fmt, raw)

    # ── catalog ──────────────────────────────────────────────────────────────

    def declare_aspect(self, name: str, physical_type: str, *, timestamp_unit: str, scale: Optional[int] = None, value_tolerance: Union[str, Decimal, int, None] = None) -> Dict[str, Any]:
        """``POST /api/v1/storage/aspects``: declare an aspect's schema (idempotent for an identical schema)."""
        payload: Dict[str, Any] = {"name": name, "physical_type": physical_type, "timestamp_unit": timestamp_unit}
        if scale is not None:
            payload["scale"] = scale
        if value_tolerance is not None:
            payload["value_tolerance"] = _decimal_text(value_tolerance)
        return self._post_json("/api/v1/storage/aspects", payload).json()

    def aspects(self) -> Dict[str, Any]:
        """``GET /api/v1/storage/aspects``: every declared schema."""
        return self._get("/api/v1/storage/aspects").json()

    def aspect_schema(self, aspect: str) -> Dict[str, Any]:
        """``GET /api/v1/storage/{aspect}/schema``."""
        return self._get(f"/api/v1/storage/{self._aspect(aspect)}/schema").json()

    def catalog(self) -> Dict[str, Any]:
        """``GET /api/v1/storage/catalog``: the store's scope and registered hierarchy."""
        return self._get("/api/v1/storage/catalog").json()

    def stats(self, aspect: Optional[str] = None) -> Dict[str, Any]:
        """``GET /api/v1/storage/stats``, or one aspect's ``…/{aspect}/stats``."""
        if aspect is None:
            return self._get("/api/v1/storage/stats").json()
        return self._get(f"/api/v1/storage/{self._aspect(aspect)}/stats").json()

    # ── ingest ───────────────────────────────────────────────────────────────

    @staticmethod
    def _idempotency(key: Optional[str]) -> Dict[str, str]:
        return {"Idempotency-Key": key} if key else {}

    def write_points(self, aspect: str, points: Iterable[Any], *, rows_per_page: Optional[int] = None, require_sorted: bool = False, idempotency_key: Optional[str] = None) -> Dict[str, Any]:
        """``POST /api/v1/storage/{aspect}/points``: seal a batch of ``(timestamp, value)`` pairs.

        With an ``idempotency_key`` a retried batch answers ``replayed: true`` instead of
        sealing twice.
        """
        payload: Dict[str, Any] = {"points": _stored_points(points), "require_sorted": require_sorted}
        if rows_per_page is not None:
            payload["rows_per_page"] = rows_per_page
        return self._post_json(f"/api/v1/storage/{self._aspect(aspect)}/points", payload, headers=self._idempotency(idempotency_key)).json()

    def write_ilp(self, aspect: str, body: Union[str, bytes], *, field: str, precision: Optional[str] = None, rows_per_page: Optional[int] = None, require_sorted: bool = False, idempotency_key: Optional[str] = None) -> Dict[str, Any]:
        """``POST /api/v1/storage/{aspect}/ilp``: seal one field of a Line Protocol body."""
        data = body.encode("utf-8") if isinstance(body, str) else body
        return self.request("POST", f"/api/v1/storage/{self._aspect(aspect)}/ilp", params={"field": field, "precision": precision, "rows_per_page": rows_per_page, "require_sorted": require_sorted}, body=data, content_type="text/plain", headers=self._idempotency(idempotency_key)).json()

    def write_csv(self, aspect: str, rows: Union[str, bytes, Iterable[Any]], *, rows_per_page: Optional[int] = None, require_sorted: bool = False, idempotency_key: Optional[str] = None) -> Dict[str, Any]:
        """``POST /api/v1/storage/{aspect}/csv``: seal ``timestamp,value`` rows (a body, or pairs to format)."""
        if isinstance(rows, (str, bytes)):
            data = rows.encode("utf-8") if isinstance(rows, str) else rows
        else:
            lines = ["timestamp,value"] + [f"{point['timestamp']},{point['value'] or ''}" for point in _stored_points(rows)]
            data = ("\n".join(lines) + "\n").encode("utf-8")
        return self.request("POST", f"/api/v1/storage/{self._aspect(aspect)}/csv", params={"rows_per_page": rows_per_page, "require_sorted": require_sorted}, body=data, content_type="text/csv", headers=self._idempotency(idempotency_key)).json()

    def write_parquet(self, aspect: str, data: Union[bytes, Any], *, rows_per_page: Optional[int] = None, require_sorted: bool = False, idempotency_key: Optional[str] = None) -> Dict[str, Any]:
        """``POST /api/v1/storage/{aspect}/parquet``: seal a Parquet file, or a ``pyarrow.Table`` written as one."""
        if not isinstance(data, (bytes, bytearray, memoryview)):
            pa = _require_pyarrow()
            import pyarrow.parquet

            sink = pa.BufferOutputStream()
            pyarrow.parquet.write_table(data, sink)
            data = sink.getvalue().to_pybytes()
        return self.request("POST", f"/api/v1/storage/{self._aspect(aspect)}/parquet", params={"rows_per_page": rows_per_page, "require_sorted": require_sorted}, body=bytes(data), content_type="application/vnd.apache.parquet", headers=self._idempotency(idempotency_key)).json()

    # ── stored reads ─────────────────────────────────────────────────────────

    def points(self, aspect: str, start: int, end: int, *, limit: Optional[int] = None, offset: Optional[int] = None, cursor: Optional[str] = None) -> Dict[str, Any]:
        """``GET /api/v1/storage/{aspect}/points``: one page of the stored window ``[start, end]``."""
        return self._get(f"/api/v1/storage/{self._aspect(aspect)}/points", start=start, end=end, limit=limit, offset=offset, cursor=cursor).json()

    def value_points(self, aspect: str, lo: StoredValue, hi: StoredValue, *, limit: Optional[int] = None, offset: Optional[int] = None, cursor: Optional[str] = None) -> Dict[str, Any]:
        """``GET /api/v1/storage/{aspect}/value-points``: one page of the stored value band ``[lo, hi]``."""
        return self._get(f"/api/v1/storage/{self._aspect(aspect)}/value-points", lo=_decimal_text(lo), hi=_decimal_text(hi), limit=limit, offset=offset, cursor=cursor).json()

    def _follow(self, fetch, page_size: int) -> Iterator[Tuple[int, Optional[Decimal]]]:
        page = fetch(None, page_size)
        while True:
            for point in page["points"]:
                value = point.get("value")
                yield point["timestamp"], (Decimal(value) if value is not None else None)
            cursor = page.get("next_cursor")
            if not cursor:
                return
            page = fetch(cursor, page_size)

    def iter_points(self, aspect: str, start: int, end: int, *, page_size: int = 10_000) -> Iterator[Tuple[int, Optional[Decimal]]]:
        """Every stored ``(timestamp, Decimal | None)`` in ``[start, end]``, following ``next_cursor``."""
        return self._follow(lambda cursor, size: self.points(aspect, start, end, limit=size, cursor=cursor), page_size)

    def iter_value_points(self, aspect: str, lo: StoredValue, hi: StoredValue, *, page_size: int = 10_000) -> Iterator[Tuple[int, Optional[Decimal]]]:
        """Every stored ``(timestamp, Decimal | None)`` whose value is in ``[lo, hi]``, following ``next_cursor``."""
        return self._follow(lambda cursor, size: self.value_points(aspect, lo, hi, limit=size, cursor=cursor), page_size)

    def at(self, aspect: str, t: int) -> Dict[str, Any]:
        """``GET /api/v1/storage/{aspect}/at``: the stored value at exactly ``t`` (``found: false`` on a miss)."""
        return self._get(f"/api/v1/storage/{self._aspect(aspect)}/at", t=t).json()

    def at_multi(self, aspect: str, instants: Iterable[int]) -> List[Dict[str, Any]]:
        """``GET /api/v1/storage/{aspect}/at-multi``: many instants in one pass, in query order."""
        return self._get(f"/api/v1/storage/{self._aspect(aspect)}/at-multi", t=",".join(str(int(t)) for t in instants)).json()["points"]

    def range(self, aspect: str, start: int, end: int, *, format: str = "arrow", raw: bool = False) -> Any:
        """The stored window ``[start, end]`` as Arrow (``…/range``), Parquet or CSV."""
        fmt = self._check_format(format, ("arrow", "parquet", "csv"))
        suffix = "" if fmt == "arrow" else f".{fmt}"
        return self._decode(self._get(f"/api/v1/storage/{self._aspect(aspect)}/range{suffix}", start=start, end=end), fmt, raw)

    def value_range(self, aspect: str, lo: StoredValue, hi: StoredValue, *, format: str = "arrow", raw: bool = False) -> Any:
        """The stored value band ``[lo, hi]`` as Arrow (``…/value-range``), Parquet or CSV."""
        fmt = self._check_format(format, ("arrow", "parquet", "csv"))
        suffix = "" if fmt == "arrow" else f".{fmt}"
        return self._decode(self._get(f"/api/v1/storage/{self._aspect(aspect)}/value-range{suffix}", lo=_decimal_text(lo), hi=_decimal_text(hi)), fmt, raw)

    def stored_downsample(self, aspect: str, *, start: Optional[int] = None, end: Optional[int] = None, resolution: Optional[str] = None, aggregations: Union[str, Sequence[str], None] = None, fill: Optional[str] = None, format: str = "json", raw: bool = False) -> Any:
        """``GET /api/v1/storage/{aspect}/downsample[.csv|.arrow|.parquet]``: reduce stored segments in place."""
        fmt = self._check_format(format)
        suffix = "" if fmt == "json" else f".{fmt}"
        return self._decode(self._get(f"/api/v1/storage/{self._aspect(aspect)}/downsample{suffix}", start=start, end=end, resolution=resolution, agg=_agg_param(aggregations), fill=fill), fmt, raw)

    # ── maintenance and backup ───────────────────────────────────────────────

    def reconcile(self, aspect: Optional[str] = None, **params: Any) -> Dict[str, Any]:
        """``POST /api/v1/storage[/{aspect}]/reconcile`` (``threshold``, ``hot_cold``, ``overlaps``, ``split_min_bytes``)."""
        path = "/api/v1/storage/reconcile" if aspect is None else f"/api/v1/storage/{self._aspect(aspect)}/reconcile"
        return self.request("POST", path, params=params).json()

    def squash(self, aspect: str, *, max_segments: Optional[int] = None) -> Dict[str, Any]:
        """``POST /api/v1/storage/{aspect}/squash``."""
        return self.request("POST", f"/api/v1/storage/{self._aspect(aspect)}/squash", params={"max_segments": max_segments}).json()

    def compact(self, aspect: str, *, target_rows: int) -> Dict[str, Any]:
        """``POST /api/v1/storage/{aspect}/compact``."""
        return self.request("POST", f"/api/v1/storage/{self._aspect(aspect)}/compact", params={"target_rows": target_rows}).json()

    def fsck(self) -> Dict[str, Any]:
        """``POST /api/v1/storage/fsck``: run the recovery pass on demand."""
        return self.request("POST", "/api/v1/storage/fsck").json()

    def quarantine(self, aspect: Optional[str] = None) -> Dict[str, Any]:
        """``GET /api/v1/storage/quarantine``: the quarantined segments, optionally for one aspect."""
        return self._get("/api/v1/storage/quarantine", aspect=aspect).json()

    def backup(self, *, label: Optional[str] = None, verify: Optional[str] = None) -> Dict[str, Any]:
        """``POST /api/v1/storage/backup``: an online control-plane snapshot (``admin``)."""
        return self.request("POST", "/api/v1/storage/backup", params={"label": label, "verify": verify}).json()

    def restore_drill(self, label: str) -> Dict[str, Any]:
        """``POST /api/v1/storage/restore/drill``: rehearse restoring ``label`` without touching the live store."""
        return self.request("POST", "/api/v1/storage/restore/drill", params={"label": label}).json()
