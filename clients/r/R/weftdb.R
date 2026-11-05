#' A connection to one weft-server.
#'
#' @param base_url The server's origin, e.g. `"http://127.0.0.1:8080"`.
#' @param api_key A key from `weft-server keygen`, sent as `Authorization: Bearer`. Never
#'   put it in the URL: the server refuses a key in a query string.
#' @param timeout Seconds per request.
#' @return A `weft_connection`.
#' @export
weft_connect <- function(base_url = "http://127.0.0.1:8080", api_key = NULL, timeout = 30) {
  stopifnot(is.character(base_url), length(base_url) == 1L)
  if (!grepl("^https?://[^/?#]+", base_url)) stop(sprintf("%s is not an http(s) origin", base_url), call. = FALSE)
  if (grepl("\\?", base_url)) stop("the base URL carries no query; a key goes in `api_key`, never the URL", call. = FALSE)
  structure(list(base_url = sub("/+$", "", base_url), api_key = api_key, timeout = timeout), class = "weft_connection")
}

#' @export
print.weft_connection <- function(x, ...) {
  cat(sprintf("<weft_connection %s%s>\n", x$base_url, if (is.null(x$api_key)) "" else " (key set)"))
  invisible(x)
}

.segment <- function(x) curl::curl_escape(x)

.query <- function(params) {
  params <- params[!vapply(params, is.null, logical(1))]
  if (!length(params)) return("")
  values <- vapply(params, function(v) {
    if (is.logical(v)) tolower(as.character(v)) else format(v, scientific = FALSE, trim = TRUE)
  }, character(1))
  paste0("?", paste(curl::curl_escape(names(values)), curl::curl_escape(values), sep = "=", collapse = "&"))
}

# One request; a refusal stops with a `weft_error` condition carrying the envelope.
.request <- function(con, method, path, params = list(), body = NULL, content_type = NULL, headers = list()) {
  handle <- curl::new_handle(customrequest = method, timeout = con$timeout, failonerror = FALSE)
  sent <- c(list(Accept = "*/*", `User-Agent` = "weftdb-r/0.1.0"), headers)
  if (!is.null(con$api_key)) sent$Authorization <- paste("Bearer", con$api_key)
  if (!is.null(body)) {
    if (is.character(body)) body <- charToRaw(enc2utf8(body))
    curl::handle_setopt(handle, post = TRUE, postfieldsize = length(body), postfields = body)
    if (!is.null(content_type)) sent$`Content-Type` <- content_type
  } else if (method == "POST") {
    curl::handle_setopt(handle, post = TRUE, postfieldsize = 0L)
  }
  do.call(curl::handle_setheaders, c(list(handle), sent))
  answer <- curl::curl_fetch_memory(paste0(con$base_url, path, .query(params)), handle = handle)
  if (answer$status_code >= 400L) .refuse(answer)
  answer
}

.refuse <- function(answer) {
  text <- rawToChar(answer$content)
  envelope <- tryCatch(jsonlite::fromJSON(text, simplifyVector = FALSE), error = function(e) list(error = trimws(text)))
  if (!is.list(envelope)) envelope <- list(error = trimws(text))
  message <- if (is.null(envelope$error) || !nzchar(envelope$error)) sprintf("HTTP %d", answer$status_code) else envelope$error
  code <- envelope$code
  condition <- structure(
    class = c(if (!is.null(code)) paste0("weft_", code), "weft_error", "error", "condition"),
    list(
      message = sprintf("%d%s: %s%s", answer$status_code, if (is.null(code)) "" else paste0(" ", code), message,
        if (is.null(envelope$request_id)) "" else sprintf(" (request %s)", envelope$request_id)),
      call = NULL, status = answer$status_code, code = code, details = envelope$details,
      request_id = envelope$request_id
    )
  )
  stop(condition)
}

.json <- function(answer, simplify = TRUE, ...) jsonlite::fromJSON(rawToChar(answer$content), simplifyVector = simplify, ...)

.arrow <- function(answer) arrow::read_ipc_stream(answer$content, as_data_frame = FALSE)

.parquet <- function(answer) arrow::read_parquet(answer$content, as_data_frame = FALSE)

#' `GET /health`: the process is up.
#' @param con A [weft_connect()] connection.
#' @export
weft_health <- function(con) .json(.request(con, "GET", "/health"))

#' `GET /ready`: whether the server can serve, with its store's identity.
#' @param con A [weft_connect()] connection.
#' @export
weft_ready <- function(con) .json(.request(con, "GET", "/ready"))

#' Every declared aspect's schema, as a data frame.
#' @param con A [weft_connect()] connection.
#' @export
weft_aspects <- function(con) {
  aspects <- .json(.request(con, "GET", "/api/v1/storage/aspects"))$aspects
  if (!length(aspects)) {
    return(data.frame(name = character(), physical_type = character(), value_tolerance = character(), timestamp_unit = character()))
  }
  aspects
}

#' Declare an aspect's schema. Declaring an identical schema again is a no-op.
#'
#' @param con A [weft_connect()] connection.
#' @param name The aspect.
#' @param physical_type `"f64"`, `"f32"`, `"scaled_i64"`, `"scaled_i128"`, `"decimal128"` or
#'   `"bigdecimal_text"`.
#' @param timestamp_unit `"seconds"`, `"millis"`, `"micros"` or `"nanos"`.
#' @param scale The fractional digits, for the fixed-point encodings.
#' @param value_tolerance The per-value error tolerance, as decimal text (`"0"` is exact).
#' @export
weft_declare <- function(con, name, physical_type, timestamp_unit, scale = NULL, value_tolerance = NULL) {
  payload <- list(name = name, physical_type = physical_type, timestamp_unit = timestamp_unit)
  if (!is.null(scale)) payload$scale <- as.integer(scale)
  if (!is.null(value_tolerance)) payload$value_tolerance <- as.character(value_tolerance)
  body <- jsonlite::toJSON(payload, auto_unbox = TRUE)
  .json(.request(con, "POST", "/api/v1/storage/aspects", body = body, content_type = "application/json"))$aspect
}

#' Seal rows into an aspect, as one Parquet batch.
#'
#' `data` is a data frame or Arrow table with an integer `timestamp` column (epoch integers
#' in the aspect's declared unit) and a `value` column. For exact storage pass `value` as
#' character decimal text: a double column is the binary number R holds, which for most
#' decimals is not the number that was typed.
#'
#' @param con A [weft_connect()] connection.
#' @param aspect The aspect.
#' @param data The rows.
#' @param idempotency_key Sent as `Idempotency-Key`: a retried batch answers
#'   `replayed = TRUE` instead of sealing twice.
#' @param require_sorted Refuse the batch unless its timestamps are non-decreasing.
#' @return The server's write summary (`row_count`, `segment_id`, `replayed`, ...).
#' @export
weft_write <- function(con, aspect, data, idempotency_key = NULL, require_sorted = FALSE) {
  table <- arrow::as_arrow_table(data)
  if (!all(c("timestamp", "value") %in% names(table))) stop("`data` needs `timestamp` and `value` columns", call. = FALSE)
  table <- table[, c("timestamp", "value")]
  if (table$timestamp$null_count > 0) stop("`timestamp` has missing values; every row needs its instant", call. = FALSE)
  if (!inherits(table$timestamp$type, "Int64")) table$timestamp <- table$timestamp$cast(arrow::int64())
  # The server reads a value column of decimal text, Float64 or Decimal128. Anything else
  # (an R integer, a factor) goes as its exact text.
  value_type <- table$value$type
  if (inherits(value_type, "DictionaryType")) table$value <- table$value$cast(value_type$value_type)
  if (!(inherits(table$value$type, "Utf8") || inherits(table$value$type, "Float64") || inherits(table$value$type, "Decimal128Type"))) {
    table$value <- table$value$cast(arrow::utf8())
  }
  sink <- arrow::BufferOutputStream$create()
  arrow::write_parquet(table, sink)
  bytes <- as.raw(sink$finish())
  headers <- if (is.null(idempotency_key)) list() else list(`Idempotency-Key` = idempotency_key)
  path <- sprintf("/api/v1/storage/%s/parquet", .segment(aspect))
  params <- list(require_sorted = if (isTRUE(require_sorted)) TRUE else NULL)
  .json(.request(con, "POST", path, params = params, body = bytes, content_type = "application/vnd.apache.parquet", headers = headers))
}

#' Every stored row in `[start, end]` as a data frame of `timestamp` (`integer64`, exact at
#' any epoch) and exact `value` (decimal text), following the server's cursor page by page.
#'
#' @param con A [weft_connect()] connection.
#' @param aspect The aspect.
#' @param start,end The inclusive window, as epoch integers in the aspect's unit.
#' @param page_size Rows per request.
#' @export
weft_points <- function(con, aspect, start, end, page_size = 10000L) {
  page_size <- suppressWarnings(as.integer(page_size))
  if (length(page_size) != 1L || is.na(page_size) || page_size < 1L) stop("`page_size` must be a positive integer", call. = FALSE)
  path <- sprintf("/api/v1/storage/%s/points", .segment(aspect))
  pages <- list()
  cursor <- NULL
  repeat {
    page <- .json(.request(con, "GET", path, params = list(start = start, end = end, limit = page_size, cursor = cursor)), simplify = FALSE, bigint_as_char = TRUE)
    pages[[length(pages) + 1L]] <- data.frame(
      timestamp = bit64::as.integer64(vapply(page$points, function(p) as.character(p$timestamp), character(1))),
      value = vapply(page$points, function(p) if (is.null(p$value)) NA_character_ else p$value, character(1)),
      stringsAsFactors = FALSE
    )
    cursor <- page$next_cursor
    if (is.null(cursor)) break
  }
  do.call(rbind, pages)
}

#' The stored window `[start, end]` as an Arrow table (the value column is exact decimal
#' text).
#'
#' @param con A [weft_connect()] connection.
#' @param aspect The aspect.
#' @param start,end The inclusive window, as epoch integers in the aspect's unit.
#' @param format `"arrow"` (an IPC stream) or `"parquet"`.
#' @export
weft_range <- function(con, aspect, start, end, format = c("arrow", "parquet")) {
  format <- match.arg(format)
  suffix <- if (format == "arrow") "" else ".parquet"
  answer <- .request(con, "GET", sprintf("/api/v1/storage/%s/range%s", .segment(aspect), suffix), params = list(start = start, end = end))
  if (format == "arrow") .arrow(answer) else .parquet(answer)
}

#' The stored rows whose value is in `[lo, hi]`, as an Arrow table.
#'
#' @param con A [weft_connect()] connection.
#' @param aspect The aspect.
#' @param lo,hi The inclusive band, as decimal text or numbers.
#' @export
weft_value_range <- function(con, aspect, lo, hi) {
  .arrow(.request(con, "GET", sprintf("/api/v1/storage/%s/value-range", .segment(aspect)), params = list(lo = as.character(lo), hi = as.character(hi))))
}

#' Reduce a stored window into epoch-aligned buckets, server-side.
#'
#' @param con A [weft_connect()] connection.
#' @param aspect The aspect.
#' @param start,end The window, as epoch integers in the aspect's unit (`NULL` for all).
#' @param resolution The bucket width: `"seconds"`, `"minutes"`, or a width like `"15m"`.
#' @param aggregations Reductions: `"avg"`, `"min"`, `"max"`, `"sum"`, `"count"`, `"p95"`, ...
#' @param fill How empty buckets are filled (`"null"`, `"prev"`, `"linear"`, ...), or `NULL`.
#' @return An Arrow table with one row per bucket.
#' @export
weft_downsample <- function(con, aspect, start = NULL, end = NULL, resolution = "minutes", aggregations = "avg", fill = NULL) {
  path <- sprintf("/api/v1/storage/%s/downsample.arrow", .segment(aspect))
  .arrow(.request(con, "GET", path, params = list(start = start, end = end, resolution = resolution, agg = paste(aggregations, collapse = ","), fill = fill)))
}

#' Interpolate a series on a regular grid (nothing is stored).
#'
#' @param con A [weft_connect()] connection.
#' @param data A data frame with `timestamp` (POSIXct) and `value` (numeric) columns.
#' @param spline `"linear"`, `"quadratic"` or `"cubic"`.
#' @param resolution The grid step: `"seconds"`, `"minutes"`, or a width like `"250ms"`.
#' @return An Arrow table of `timestamp`, `value` and `kind` (raw, interpolated or
#'   extrapolated).
#' @export
weft_interpolate <- function(con, data, spline = "cubic", resolution = "seconds") {
  # Whole microseconds, rounded (strftime's %OS6 truncates), with the fraction formatted
  # from an integer; a microsecond epoch stays below 2^53, so this is exact.
  micros <- round(as.numeric(as.POSIXct(data$timestamp, tz = "UTC")) * 1e6)
  whole <- floor(micros / 1e6)
  points <- data.frame(
    timestamp = paste0(format(as.POSIXct(whole, origin = "1970-01-01", tz = "UTC"), "%Y-%m-%dT%H:%M:%S", tz = "UTC"), sprintf(".%06.0fZ", micros - whole * 1e6)),
    value = as.numeric(data$value)
  )
  body <- jsonlite::toJSON(list(spline = spline, resolution = resolution, points = points), auto_unbox = TRUE, digits = NA)
  .arrow(.request(con, "POST", "/api/v1/interpolate/arrow", body = body, content_type = "application/json"))
}
