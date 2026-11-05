# weftdb (R)

An R client for [WeftDB](https://github.com/basic-automation/weftdb)'s `weft-server`, built on
Apache Arrow: stored windows, value bands and server-side downsampling come back as Arrow
tables, and data frames or Arrow tables go in as one Parquet batch.

```r
# install.packages(c("arrow", "curl", "jsonlite")); then, from a checkout:
# install.packages("clients/r", repos = NULL, type = "source")
library(weftdb)
con <- weft_connect("http://127.0.0.1:8080")   # api_key = "weft_…" when auth is on

weft_declare(con, "price", "scaled_i64", "millis", scale = 2)
weft_write(con, "price", data.frame(timestamp = c(1000, 2000), value = c("1.25", "2.50")),
           idempotency_key = "batch-1")

weft_range(con, "price", 0, 9999)                     # Arrow Table: timestamp, value (exact text)
weft_points(con, "price", 0, 9999)                    # data.frame, following the cursor
weft_downsample(con, "price", resolution = "15m",     # reduced on the server
                aggregations = c("avg", "p95"))
dplyr::collect(weft_range(con, "price", 0, 9999))     # into dplyr, or arrow's own verbs
```

Values are stored exactly: pass `value` as character decimal text to keep every digit (a
double column is R's binary approximation). Refusals are conditions of class `weft_error` and
`weft_<code>` (e.g. `weft_aspect_not_found`), carrying `status`, `code`, `details` and
`request_id`.

For a long window, Arrow Flight streams the same batches without one large HTTP body: start
the server with `WEFT_FLIGHT_ADDR=127.0.0.1:8815` and use `arrow::flight_connect()` (it
needs `pyarrow` through `reticulate`) with a JSON ticket such as
`{"aspect":"price","start":0,"end":9999}`.

Tests: `testthat` offline checks always run; set `WEFT_SERVER_URL` to a running server to run
the live contract (`tests/testthat/test-live.R`).
