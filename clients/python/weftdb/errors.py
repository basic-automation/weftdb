"""The error types: one per class of the server's error envelope.

Every refusal the server sends has one JSON shape,
``{"error", "code", "details"?, "limit"?, "committed"?, "request_id"?}``. A client
branches on ``code`` (stable) rather than on the sentence (which may change), so
:class:`WeftError` carries every field, and the subclasses only group codes by what a
caller usually does about them.
"""

from __future__ import annotations

from typing import Any, Optional


class WeftError(Exception):
    """A request the server refused, or a response the client could not use."""

    def __init__(
        self,
        message: str,
        *,
        status: Optional[int] = None,
        code: Optional[str] = None,
        details: Any = None,
        limit: Any = None,
        committed: Optional[bool] = None,
        request_id: Optional[str] = None,
        retry_after: Optional[float] = None,
    ) -> None:
        super().__init__(message)
        self.message = message
        self.status = status
        self.code = code
        self.details = details
        self.limit = limit
        self.committed = committed
        self.request_id = request_id
        self.retry_after = retry_after

    def __str__(self) -> str:
        parts = [self.message]
        if self.code:
            parts.append(f"[{self.status} {self.code}]")
        if self.request_id:
            parts.append(f"(request {self.request_id})")
        return " ".join(parts)


class BadRequest(WeftError):
    """``400``/``408``/``413``/``415``/``422``: change the request before sending it again."""


class Unauthenticated(WeftError):
    """``401``: no API key, or one the server does not know."""


class Forbidden(WeftError):
    """``403``: the key's role does not hold the route's scope, or a host/origin guard refused."""


class NotFound(WeftError):
    """``404``: no such route, aspect, run or quarantine record."""


class Conflict(WeftError):
    """``409``/``410``: a schema or idempotency conflict, a busy maintenance lane, a stale cursor."""


class Overloaded(WeftError):
    """``503 overloaded``: the admission class is full; ``retry_after`` says when to retry."""


class ServerError(WeftError):
    """Any other ``5xx``. The body names only the condition; the cause is in the server log."""


class IntegrityError(WeftError):
    """A response that does not match what it claims to be (a bench run whose bytes do not hash to its id)."""


def error_class(status: int, code: Optional[str]) -> type:
    """The exception class for a refusal's status and code."""
    if code == "overloaded":
        return Overloaded
    if status == 401:
        return Unauthenticated
    if status == 403:
        return Forbidden
    if status == 404:
        return NotFound
    if status in (409, 410):
        return Conflict
    if status >= 500:
        return ServerError
    if status >= 400:
        return BadRequest
    return WeftError
