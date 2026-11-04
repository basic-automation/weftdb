test_that("a connection is an origin without secrets", {
  con <- weft_connect("http://127.0.0.1:1/")
  expect_equal(con$base_url, "http://127.0.0.1:1")
  expect_error(weft_connect("127.0.0.1:8080"), "not an http")
  expect_error(weft_connect("http://h/?api_key=weft_x"), "carries no query")
  expect_output(print(weft_connect("http://h", api_key = "weft_secret")), "key set")
  expect_false(grepl("weft_secret", capture.output(print(weft_connect("http://h", api_key = "weft_secret")))))
})

test_that("query strings encode every value", {
  expect_equal(weftdb:::.query(list(start = 0, end = 1e12, cursor = NULL, sorted = TRUE)), "?start=0&end=1000000000000&sorted=true")
  expect_equal(weftdb:::.query(list(lo = "-1.5", agg = "avg,max")), "?lo=-1.5&agg=avg%2Cmax")
  expect_equal(weftdb:::.segment("a/b c"), "a%2Fb%20c")
})
