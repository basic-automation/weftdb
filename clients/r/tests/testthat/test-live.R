# The contract against a running weft-server: set WEFT_SERVER_URL to one with a store
# (e.g. http://127.0.0.1:8080). Each run declares fresh aspect names, so a reused server works.
url <- Sys.getenv("WEFT_SERVER_URL")
skip_if(!nzchar(url), "set WEFT_SERVER_URL to run the live contract")

con <- weft_connect(url)
suffix <- format(as.numeric(Sys.time()) * 1e6, scientific = FALSE)

test_that("probes answer", {
  expect_true(weft_ready(con)$ready)
  expect_true("status" %in% names(weft_health(con)))
})

test_that("a series round-trips exactly through Arrow", {
  aspect <- paste0("r_price_", suffix)
  info <- weft_declare(con, aspect, "scaled_i64", "millis", scale = 2, value_tolerance = "0")
  expect_equal(info$scale, 2)
  written <- weft_write(con, aspect, data.frame(timestamp = c(1000, 2000, 3000), value = c("1.25", "2.50", "3")), idempotency_key = paste0("r-", suffix))
  expect_equal(written$row_count, 3)
  replay <- weft_write(con, aspect, data.frame(timestamp = c(1000, 2000, 3000), value = c("1.25", "2.50", "3")), idempotency_key = paste0("r-", suffix))
  expect_true(replay$replayed)

  rows <- weft_points(con, aspect, 0, 9999, page_size = 2)
  expect_equal(as.character(rows$timestamp), c("1000", "2000", "3000"))
  expect_error(weft_points(con, aspect, 0, 9999, page_size = 0), "positive integer")
  expect_equal(rows$value, c("1.25", "2.50", "3.00"))

  table <- weft_range(con, aspect, 0, 9999)
  expect_s3_class(table, "Table")
  expect_equal(table$num_rows, 3)
  expect_equal(as.numeric(as.vector(table$timestamp)), c(1000, 2000, 3000))
  expect_equal(weft_range(con, aspect, 0, 9999, format = "parquet")$num_rows, 3)
  expect_equal(weft_value_range(con, aspect, "2", "4")$num_rows, 2)
  expect_true(aspect %in% weft_aspects(con)$name)

  down <- as.data.frame(weft_downsample(con, aspect, 0, 9999, resolution = "seconds", aggregations = c("avg", "max")))
  expect_equal(nrow(down), 3)
  expect_true(all(c("avg", "max") %in% names(down)))
})

test_that("integer values and epochs past 2^53 travel exactly", {
  aspect <- paste0("r_wide_", suffix)
  weft_declare(con, aspect, "scaled_i64", "nanos", scale = 0)
  big <- bit64::as.integer64("1700000000123456789")
  weft_write(con, aspect, arrow::arrow_table(timestamp = c(big, big + 1L), value = c(7L, 8L)))
  rows <- weft_points(con, aspect, big, big + 1L)
  expect_equal(as.character(rows$timestamp), c("1700000000123456789", "1700000000123456790"))
  expect_equal(rows$value, c("7", "8"))
  expect_error(weft_write(con, aspect, data.frame(timestamp = c(1, NA), value = c("1", "2"))), "missing values")
})

test_that("refusals carry the envelope code", {
  err <- tryCatch(weft_range(con, paste0("never_", suffix), 0, 1), weft_error = function(e) e)
  expect_s3_class(err, "weft_aspect_not_found")
  expect_equal(err$status, 404)
  expect_true(nzchar(err$request_id))
  aspect <- paste0("r_exact_", suffix)
  weft_declare(con, aspect, "scaled_i64", "millis", scale = 2)
  expect_error(weft_write(con, aspect, data.frame(timestamp = 1, value = "1.234")), class = "weft_value_not_representable")
})

test_that("interpolation answers a grid with provenance", {
  t0 <- as.POSIXct("1970-01-01 00:00:00", tz = "UTC")
  grid <- as.data.frame(weft_interpolate(con, data.frame(timestamp = t0 + c(0, 30, 60), value = c(0, 30, 60)), spline = "linear"))
  expect_equal(nrow(grid), 61)
  expect_setequal(unique(grid$kind), c("raw", "interpolated"))
  # A sub-second instant is rounded to the microsecond, not truncated: 0.0000005 s apart
  # from the whole second would otherwise land a microsecond early.
  odd <- as.data.frame(weft_interpolate(con, data.frame(timestamp = t0 + c(0, 1.0000006), value = c(0, 1)), spline = "linear", resolution = "microseconds"))
  expect_equal(nrow(odd), 1000002)
})
