# weftdb — Python client

A client for `weft-server`'s HTTP API. It is standard-library only; `pyarrow` (the `arrow`
extra) decodes the Arrow IPC and Parquet answers.

```bash
pip install ./clients/python            # or: pip install './clients/python[arrow]'
```

```python
from decimal import Decimal
from weftdb import Client

db = Client("http://127.0.0.1:8080", api_key=None)   # api_key="weft_…" when auth is on

# Interpolate without storing anything (compute endpoints speak f64 and RFC 3339).
grid = db.interpolate([("1970-01-01T00:00:00Z", 0.0), ("1970-01-01T00:01:00Z", 60.0)],
                      spline="linear", resolution="seconds")
grid["points"][1]        # {'timestamp': '1970-01-01T00:00:01Z', 'value': 1.0, 'kind': 'interpolated'}

# Store a series and read it back (storage endpoints are lossless: integers + decimal text).
db.declare_aspect("price", "scaled_i64", scale=2, value_tolerance="0", timestamp_unit="millis")
db.write_points("price", [(1000, Decimal("1.25")), (2000, "2.50")], idempotency_key="batch-1")
list(db.iter_points("price", 0, 9999))     # [(1000, Decimal('1.25')), (2000, Decimal('2.50'))]
db.at("price", 2000)                        # {'found': True, 'value': '2.50', ...}
db.stored_downsample("price", resolution="hours", aggregations=["avg", "p99"])

# Columnar: a pyarrow.Table (or raw=True for the bytes).
table = db.range("price", 0, 9999, format="parquet")
```

## Conventions

- **Lossless storage.** Stored timestamps are integers in the aspect's declared unit, and
  values go in as `Decimal`, `str` or `int`. A `float` is refused before anything is sent:
  `1.1` is not the number its binary expansion holds, and the server would reject what it
  cannot represent anyway (`422 value_not_representable`).
- **Errors.** A refusal raises a `WeftError` subclass: `BadRequest`, `Unauthenticated`,
  `Forbidden`, `NotFound`, `Conflict`, `Overloaded`, `ServerError`. Each carries the
  envelope's `code`, `details`, `limit`, `committed` and `request_id`. Branch on `code`.
- **Retries.** A `GET` refused `503 overloaded` is retried after `Retry-After`
  (`max_retries`, default 2). Writes are not retried by the client; send them with an
  `idempotency_key` and a retry answers `replayed: true` instead of sealing twice.
- **Credentials** go in `api_key` (a Bearer header). The server refuses a key in the URL.
- **Bench runs.** `bench_run(id)` checks that the answer's bytes hash to `id`, and raises
  `IntegrityError` if they do not.

## Tests

```bash
python3 -m unittest discover -s clients/python/tests -p 'test_*.py'
```

That runs the unit tests against an in-process fake. To run the contract tests against a
real server too, build one and name it:

```bash
cargo build -p weft-server && WEFT_SERVER_BIN=target/debug/weft-server \
  python3 -m unittest discover -s clients/python/tests -p 'test_*.py'
```
