"""Python client for the WeftDB HTTP API (``weft-server``).

>>> from weftdb import Client
>>> db = Client("http://127.0.0.1:8080")
>>> db.declare_aspect("price", "scaled_i64", scale=2, value_tolerance="0", timestamp_unit="millis")
>>> db.write_points("price", [(1000, "1.25"), (2000, "2.50")])
>>> list(db.iter_points("price", 0, 9999))
[(1000, Decimal('1.25')), (2000, Decimal('2.50'))]
"""

__version__ = "0.1.0"

from .client import Client, Response, decode_arrow, decode_parquet  # noqa: E402
from .errors import BadRequest, Conflict, Forbidden, IntegrityError, NotFound, Overloaded, ServerError, Unauthenticated, WeftError  # noqa: E402

__all__ = ["Client", "Response", "decode_arrow", "decode_parquet", "WeftError", "BadRequest", "Unauthenticated", "Forbidden", "NotFound", "Conflict", "Overloaded", "ServerError", "IntegrityError", "__version__"]
