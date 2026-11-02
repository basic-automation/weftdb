# weft-bench in Grafana

`weft-bench.json` is a Grafana dashboard over the Prometheus text a run writes with `--prom`
(or `weft-bench export REPORT.json --format prom`). Each result becomes four series labelled with
its `profile`, `workload` and `adapter`: `weft_bench_latency_seconds` (quantiles 0.5, 0.95, 0.99),
`weft_bench_throughput_per_second`, `weft_bench_reps` and `weft_bench_publishable` (1 when the
result passed its correctness gate). The latency and throughput panels show only publishable
results; the first two panels list the ones that are not, so a failing gate is never hidden behind
a fast number.

To collect them, write each run's `.prom` into a node-exporter textfile-collector directory
(`--collector.textfile.directory`), or push it to a Pushgateway:

```bash
curl --data-binary @report.prom http://pushgateway:9091/metrics/job/weft-bench
```

Then import `weft-bench.json` in Grafana and pick the Prometheus data source.
