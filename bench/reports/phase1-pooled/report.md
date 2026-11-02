# WeftDB cross-engine benchmark

Generated 2026-11-02T18:07:10.047287125+00:00 on AMD Ryzen 9 7950X3D 16-Core Processor (linux x86_64). Engines: weftdb 0.1.0, duckdb v2.0.0, clickhouse 26.10.3.7, timescaledb 2.31.0 (PostgreSQL 17.11), questdb 10.1.0 (commit mode sync), influxdb3 InfluxDB 3 Core 3.12.2, iotdb 2.0.11. Source commit: 4531d48347167ad4d9749fec45dcc3a060ec7316. Seed: 1592649491.

**At least one result failed its correctness gate. Its timing is not a valid comparison; see the failures below.**

## Results

Latency in milliseconds: p50 with its bootstrap interval, and p95. Accuracy is RMSE against the analytic ground truth, for synthetic datasets. B/pt is the store's size on disk after a load, divided by the points loaded (whole files: indexes and headers included). A phase marked `os-cold` ran each rep after the OS page cache was dropped, and one marked `restarted` after the engine restarted as well; unmarked phases are warm, back-to-back reps.

| dataset | phase | arm | class | input | p50 [CI] | p95 | reps | gate | RMSE | B/pt |
| --- | --- | --- | --- | --- | --- | --- | --- | --- | --- | --- |
| mcar-small | interpolate | weftdb-memory | A · native | 86245 | 21.038 [20.555, 23.052] | 24.295 | 20 | PASS | 0.9430 | — |
| mcar-small | downsample | weftdb-memory | A · native | 86245 | 1.676 [1.657, 1.697] | 1.719 | 20 | PASS | — | — |
| mcar-small | load | weftdb-stored | A · native | 86245 | 910.332 [888.116, 926.149] | 974.335 | 6 | — | — | 7.98 |
| mcar-small | interpolate | weftdb-stored | A · native | 86245 | 26.247 [25.947, 27.180] | 29.080 | 20 | PASS | 0.9430 | — |
| mcar-small | downsample | weftdb-stored | A · native | 86245 | 4.019 [3.823, 4.179] | 4.382 | 20 | PASS | — | — |
| mcar-small | load | weftdb-stored-f64 | A · native | 86245 | 897.594 [886.717, 900.041] | 912.970 | 6 | — | — | 7.98 |
| mcar-small | interpolate | weftdb-stored-f64 | A · native | 86245 | 8.910 [8.130, 10.677] | 10.804 | 20 | PASS | 0.9430 | — |
| mcar-small | load | weftdb-stored-bulk | A · native | 86245 | 305.404 [302.807, 316.666] | 316.666 | 6 | — | — | 7.48 |
| mcar-small | interpolate | weftdb-stored-bulk | A · native | 86245 | 27.742 [26.887, 29.395] | 30.801 | 20 | PASS | 0.9430 | — |
| mcar-small | downsample | weftdb-stored-bulk | A · native | 86245 | 4.048 [3.438, 4.598] | 4.875 | 20 | PASS | — | — |
| mcar-small | load | duckdb-double | B · portable SQL | 86245 | 46.406 [44.856, 47.831] | 50.602 | 6 | — | — | 12.30 |
| mcar-small | interpolate | duckdb-double | B · portable SQL | 86245 | 24.475 [23.925, 25.686] | 27.644 | 20 | PASS | 0.9430 | — |
| mcar-small | downsample | duckdb-double | B · portable SQL | 86245 | 2.080 [1.860, 2.249] | 3.111 | 20 | PASS | — | — |
| mcar-small | load | duckdb-decimal | B · portable SQL | 86245 | 53.108 [51.679, 54.136] | 58.844 | 6 | — | — | 12.30 |
| mcar-small | interpolate | duckdb-decimal | B · portable SQL | 86245 | 25.842 [24.698, 27.243] | 29.461 | 20 | PASS | 0.9430 | — |
| mcar-small | downsample | duckdb-decimal | B · portable SQL | 86245 | 2.139 [2.044, 2.323] | 3.261 | 20 | PASS | — | — |
| mcar-small | interpolate | duckdb-client | C · client-side | 86245 | 9.098 [8.172, 9.462] | 10.060 | 20 | PASS | 0.9430 | — |
| mcar-small | load | clickhouse-double | B · portable SQL | 86245 | 34.519 [33.802, 37.681] | 37.681 | 6 | — | — | 19.11 |
| mcar-small | interpolate | clickhouse-double | B · portable SQL | 86245 | 76.994 [70.849, 83.226] | 90.481 | 20 | PASS | 0.9430 | — |
| mcar-small | downsample | clickhouse-double | B · portable SQL | 86245 | 3.055 [2.787, 3.295] | 3.975 | 20 | PASS | — | — |
| mcar-small | load | clickhouse-decimal | B · portable SQL | 86245 | 35.356 [33.839, 101.102] | 101.102 | 6 | — | — | 16.36 |
| mcar-small | interpolate | clickhouse-decimal | B · portable SQL | 86245 | 79.517 [73.435, 85.329] | 89.715 | 20 | PASS | 0.9430 | — |
| mcar-small | downsample | clickhouse-decimal | B · portable SQL | 86245 | 3.044 [2.576, 3.256] | 3.842 | 20 | PASS | — | — |
| mcar-small | interpolate | clickhouse-double-merge | B · portable SQL | 86245 | 82.669 [77.321, 85.367] | 90.090 | 20 | PASS | 0.9430 | — |
| mcar-small | interpolate | clickhouse-native | A · native | 86245 | 21.003 [20.173, 21.151] | 22.059 | 20 | own answer | 1.0586 | — |
| mcar-small | interpolate | clickhouse-client | C · client-side | 86245 | 5.534 [5.189, 6.309] | 6.480 | 20 | PASS | 0.9430 | — |
| mcar-small | load | timescaledb-double | B · portable SQL | 86245 | 153.592 [148.772, 157.911] | 157.911 | 6 | — | — | 95.94 |
| mcar-small | interpolate | timescaledb-double | B · portable SQL | 86245 | 477.924 [469.024, 480.215] | 488.147 | 20 | PASS | 0.9430 | — |
| mcar-small | downsample | timescaledb-double | B · portable SQL | 86245 | 11.276 [11.142, 11.430] | 12.341 | 20 | PASS | — | — |
| mcar-small | load | timescaledb-numeric | B · portable SQL | 86245 | 139.348 [133.073, 142.787] | 142.787 | 6 | — | — | 104.58 |
| mcar-small | interpolate | timescaledb-numeric | B · portable SQL | 86245 | 531.527 [520.312, 536.888] | 555.982 | 20 | PASS | 0.9430 | — |
| mcar-small | downsample | timescaledb-numeric | B · portable SQL | 86245 | 16.424 [15.858, 16.719] | 17.386 | 20 | PASS | — | — |
| mcar-small | interpolate | timescaledb-native | A · native | 86245 | 73.610 [71.243, 76.353] | 77.888 | 20 | own answer | 1.0031 | — |
| mcar-small | interpolate | timescaledb-client | C · client-side | 86245 | 20.295 [20.150, 20.979] | 22.324 | 20 | PASS | 0.9430 | — |
| mcar-small | load | questdb-double | B · portable SQL | 86245 | 77.388 [75.468, 92.168] | 92.168 | 6 | — | — | 583.59 |
| mcar-small | interpolate | questdb-double | B · portable SQL | 86245 | 179.369 [176.765, 186.554] | 211.543 | 20 | PASS | 0.9430 | — |
| mcar-small | downsample | questdb-double | B · portable SQL | 86245 | 2.394 [2.328, 2.473] | 4.293 | 20 | PASS | — | — |
| mcar-small | load | questdb-decimal | B · portable SQL | 86245 | 81.879 [77.230, 83.399] | 83.399 | 6 | — | — | 583.59 |
| mcar-small | interpolate | questdb-decimal | B · portable SQL | 86245 | 178.661 [174.935, 182.673] | 197.238 | 20 | PASS | 0.9430 | — |
| mcar-small | downsample | questdb-decimal | B · portable SQL | 86245 | 2.925 [2.854, 3.101] | 5.208 | 20 | PASS | — | — |
| mcar-small | interpolate | questdb-double-linear | B · portable SQL | 86245 | 178.721 [176.392, 189.016] | 195.166 | 20 | PASS | 0.9430 | — |
| mcar-small | interpolate | questdb-native | A · native | 86245 | 14.672 [14.334, 15.206] | 16.154 | 20 | own answer | 1.0031 | — |
| mcar-small | interpolate | questdb-client | C · client-side | 86245 | 9.360 [8.849, 9.410] | 10.297 | 20 | PASS | 0.9430 | — |
| mcar-small | load | influxdb3-double | B · portable SQL | 86245 | 997.537 [366.123, 1000.103] | 1133.957 | 6 | — | — | — |
| mcar-small | interpolate | influxdb3-double | B · portable SQL | 86245 | 128.466 [113.676, 152.568] | 153.935 | 20 | PASS | 0.9430 | — |
| mcar-small | downsample | influxdb3-double | B · portable SQL | 86245 | 9.190 [8.703, 9.449] | 10.255 | 20 | PASS | — | — |
| mcar-small | interpolate | influxdb3-native | A · native | 86245 | 27.793 [26.996, 29.167] | 31.575 | 20 | own answer | 1.0031 | — |
| mcar-small | interpolate | influxdb3-client | C · client-side | 86245 | 15.526 [14.663, 16.152] | 16.704 | 20 | PASS | 0.9430 | — |
| mcar-small | load | iotdb-double | B · portable SQL | 86245 | 279.266 [210.188, 336.374] | 336.374 | 6 | — | — | 11.08 |
| mcar-small | interpolate | iotdb-double | B · portable SQL | 86245 | 91.252 [83.475, 275.368] | 278.276 | 20 | PASS | 0.9430 | — |
| mcar-small | downsample | iotdb-double | B · portable SQL | 86245 | 12.388 [11.861, 28.754] | 29.059 | 20 | PASS | — | — |
| mcar-small | interpolate | iotdb-native | A · native | 86245 | 68.810 [62.391, 171.542] | 177.035 | 20 | own answer | 1.0031 | — |
| mcar-small | interpolate | iotdb-client | C · client-side | 86245 | 49.812 [46.610, 85.649] | 86.560 | 20 | PASS | 0.9430 | — |
| block-small | interpolate | weftdb-memory | A · native | 68751 | 18.856 [18.487, 19.012] | 19.875 | 20 | PASS | 0.9504 | — |
| block-small | downsample | weftdb-memory | A · native | 68751 | 1.402 [1.373, 1.444] | 1.547 | 20 | PASS | — | — |
| block-small | load | weftdb-stored | A · native | 68751 | 765.650 [750.745, 785.202] | 786.887 | 6 | — | — | 8.05 |
| block-small | interpolate | weftdb-stored | A · native | 68751 | 22.706 [22.167, 24.168] | 25.099 | 20 | PASS | 0.9504 | — |
| block-small | downsample | weftdb-stored | A · native | 68751 | 3.433 [3.296, 3.544] | 3.818 | 20 | PASS | — | — |
| block-small | load | weftdb-stored-f64 | A · native | 68751 | 793.051 [758.936, 811.900] | 811.900 | 6 | — | — | 8.05 |
| block-small | interpolate | weftdb-stored-f64 | A · native | 68751 | 8.324 [7.537, 8.825] | 9.279 | 20 | PASS | 0.9504 | — |
| block-small | load | weftdb-stored-bulk | A · native | 68751 | 288.435 [283.686, 291.728] | 292.664 | 6 | — | — | 7.56 |
| block-small | interpolate | weftdb-stored-bulk | A · native | 68751 | 22.613 [22.249, 23.107] | 23.403 | 20 | PASS | 0.9504 | — |
| block-small | downsample | weftdb-stored-bulk | A · native | 68751 | 3.219 [3.110, 3.626] | 3.855 | 20 | PASS | — | — |
| block-small | load | duckdb-double | B · portable SQL | 68751 | 41.738 [40.556, 43.553] | 44.192 | 6 | — | — | 15.43 |
| block-small | interpolate | duckdb-double | B · portable SQL | 68751 | 23.080 [23.017, 23.477] | 25.949 | 20 | PASS | 0.9504 | — |
| block-small | downsample | duckdb-double | B · portable SQL | 68751 | 1.891 [1.783, 2.089] | 2.957 | 20 | PASS | — | — |
| block-small | load | duckdb-decimal | B · portable SQL | 68751 | 45.935 [45.300, 49.175] | 49.175 | 6 | — | — | 15.43 |
| block-small | interpolate | duckdb-decimal | B · portable SQL | 68751 | 23.792 [22.768, 24.691] | 29.150 | 20 | PASS | 0.9504 | — |
| block-small | downsample | duckdb-decimal | B · portable SQL | 68751 | 1.854 [1.739, 1.968] | 2.828 | 20 | PASS | — | — |
| block-small | interpolate | duckdb-client | C · client-side | 68751 | 7.974 [7.212, 8.410] | 9.304 | 20 | PASS | 0.9504 | — |
| block-small | load | clickhouse-double | B · portable SQL | 68751 | 34.266 [32.051, 35.490] | 36.013 | 6 | — | — | 19.32 |
| block-small | interpolate | clickhouse-double | B · portable SQL | 68751 | 65.828 [62.978, 71.011] | 73.026 | 20 | PASS | 0.9504 | — |
| block-small | downsample | clickhouse-double | B · portable SQL | 68751 | 2.623 [2.539, 2.703] | 3.686 | 20 | PASS | — | — |
| block-small | load | clickhouse-decimal | B · portable SQL | 68751 | 32.899 [31.244, 33.282] | 33.282 | 6 | — | — | 16.55 |
| block-small | interpolate | clickhouse-decimal | B · portable SQL | 68751 | 68.370 [65.856, 70.071] | 75.663 | 20 | PASS | 0.9504 | — |
| block-small | downsample | clickhouse-decimal | B · portable SQL | 68751 | 2.445 [2.269, 2.711] | 3.563 | 20 | PASS | — | — |
| block-small | interpolate | clickhouse-double-merge | B · portable SQL | 68751 | 72.095 [70.677, 73.701] | 76.636 | 20 | PASS | 0.9504 | — |
| block-small | interpolate | clickhouse-native | A · native | 68751 | 23.481 [23.046, 23.708] | 24.689 | 20 | own answer | 1.4646 | — |
| block-small | interpolate | clickhouse-client | C · client-side | 68751 | 5.050 [4.747, 5.502] | 5.846 | 20 | PASS | 0.9504 | — |
| block-small | load | timescaledb-double | B · portable SQL | 68751 | 126.235 [124.365, 131.393] | 139.097 | 6 | — | — | 100.69 |
| block-small | interpolate | timescaledb-double | B · portable SQL | 68751 | 472.848 [469.730, 477.526] | 479.884 | 20 | PASS | 0.9504 | — |
| block-small | downsample | timescaledb-double | B · portable SQL | 68751 | 9.117 [9.003, 9.264] | 10.232 | 20 | PASS | — | — |
| block-small | load | timescaledb-numeric | B · portable SQL | 68751 | 116.348 [113.801, 117.852] | 127.535 | 6 | — | — | 111.53 |
| block-small | interpolate | timescaledb-numeric | B · portable SQL | 68751 | 527.687 [522.971, 531.298] | 539.358 | 20 | PASS | 0.9504 | — |
| block-small | downsample | timescaledb-numeric | B · portable SQL | 68751 | 13.428 [12.754, 13.879] | 14.182 | 20 | PASS | — | — |
| block-small | interpolate | timescaledb-native | A · native | 68751 | 58.735 [55.123, 62.025] | 64.609 | 20 | own answer | 0.9623 | — |
| block-small | interpolate | timescaledb-client | C · client-side | 68751 | 16.831 [16.312, 18.113] | 19.911 | 20 | PASS | 0.9504 | — |
| block-small | load | questdb-double | B · portable SQL | 68751 | 73.564 [72.398, 76.959] | 83.039 | 6 | — | — | 732.09 |
| block-small | interpolate | questdb-double | B · portable SQL | 68751 | 153.732 [152.015, 161.523] | 167.367 | 20 | PASS | 0.9504 | — |
| block-small | downsample | questdb-double | B · portable SQL | 68751 | 2.000 [1.909, 2.334] | 4.516 | 20 | PASS | — | — |
| block-small | load | questdb-decimal | B · portable SQL | 68751 | 76.702 [73.813, 80.541] | 90.779 | 6 | — | — | 732.09 |
| block-small | interpolate | questdb-decimal | B · portable SQL | 68751 | 153.055 [144.584, 157.374] | 182.172 | 20 | PASS | 0.9504 | — |
| block-small | downsample | questdb-decimal | B · portable SQL | 68751 | 2.295 [2.245, 2.476] | 4.386 | 20 | PASS | — | — |
| block-small | interpolate | questdb-double-linear | B · portable SQL | 68751 | 153.792 [149.185, 160.493] | 177.901 | 20 | PASS | 0.9504 | — |
| block-small | interpolate | questdb-native | A · native | 68751 | 13.877 [13.804, 14.135] | 15.313 | 20 | own answer | 0.9623 | — |
| block-small | interpolate | questdb-client | C · client-side | 68751 | 7.232 [7.015, 7.540] | 8.041 | 20 | PASS | 0.9504 | — |
| block-small | load | influxdb3-double | B · portable SQL | 68751 | 878.327 [651.977, 1080.013] | 1080.013 | 6 | — | — | — |
| block-small | interpolate | influxdb3-double | B · portable SQL | 68751 | 115.557 [107.791, 129.842] | 139.266 | 20 | PASS | 0.9504 | — |
| block-small | downsample | influxdb3-double | B · portable SQL | 68751 | 8.502 [8.135, 8.636] | 9.352 | 20 | PASS | — | — |
| block-small | interpolate | influxdb3-native | A · native | 68751 | 23.915 [23.376, 25.148] | 26.533 | 20 | own answer | 0.9623 | — |
| block-small | interpolate | influxdb3-client | C · client-side | 68751 | 15.532 [14.834, 15.913] | 17.625 | 20 | PASS | 0.9504 | — |
| block-small | load | iotdb-double | B · portable SQL | 68751 | 248.486 [218.069, 351.293] | 351.293 | 6 | — | — | 10.96 |
| block-small | interpolate | iotdb-double | B · portable SQL | 68751 | 80.952 [75.569, 232.893] | 237.533 | 20 | PASS | 0.9504 | — |
| block-small | downsample | iotdb-double | B · portable SQL | 68751 | 11.241 [10.472, 26.595] | 27.524 | 20 | PASS | — | — |
| block-small | interpolate | iotdb-native | A · native | 68751 | 64.552 [59.714, 128.607] | 141.689 | 20 | own answer | 0.9623 | — |
| block-small | interpolate | iotdb-client | C · client-side | 68751 | 39.981 [37.688, 55.492] | 63.145 | 20 | PASS | 0.9504 | — |
| mcar-medium | interpolate | weftdb-memory | A · native | 604689 | 141.324 [136.726, 147.831] | 148.617 | 20 | PASS | 0.9407 | — |
| mcar-medium | downsample | weftdb-memory | A · native | 604689 | 7.683 [7.479, 8.962] | 9.280 | 20 | PASS | — | — |
| mcar-medium | load | weftdb-stored | A · native | 604689 | 5134.862 [5086.870, 5210.488] | 5249.037 | 6 | — | — | 7.17 |
| mcar-medium | interpolate | weftdb-stored | A · native | 604689 | 177.728 [173.097, 209.990] | 211.421 | 20 | PASS | 0.9407 | — |
| mcar-medium | downsample | weftdb-stored | A · native | 604689 | 17.094 [16.481, 18.432] | 19.920 | 20 | PASS | — | — |
| mcar-medium | load | weftdb-stored-f64 | A · native | 604689 | 5227.047 [5136.176, 5333.270] | 5369.573 | 6 | — | — | 7.17 |
| mcar-medium | interpolate | weftdb-stored-f64 | A · native | 604689 | 65.351 [59.928, 71.107] | 76.561 | 20 | PASS | 0.9407 | — |
| mcar-medium | load | weftdb-stored-bulk | A · native | 604689 | 964.077 [957.507, 983.608] | 983.608 | 6 | — | — | 6.66 |
| mcar-medium | interpolate | weftdb-stored-bulk | A · native | 604689 | 174.457 [169.953, 183.249] | 187.428 | 20 | PASS | 0.9407 | — |
| mcar-medium | downsample | weftdb-stored-bulk | A · native | 604689 | 27.480 [25.816, 28.540] | 29.032 | 20 | PASS | — | — |
| mcar-medium | load | duckdb-double | B · portable SQL | 604689 | 183.078 [181.256, 187.364] | 187.364 | 6 | — | — | 8.26 |
| mcar-medium | interpolate | duckdb-double | B · portable SQL | 604689 | 49.438 [45.675, 51.711] | 54.083 | 20 | PASS | 0.9407 | — |
| mcar-medium | downsample | duckdb-double | B · portable SQL | 604689 | 5.090 [4.764, 6.404] | 6.980 | 20 | PASS | — | — |
| mcar-medium | load | duckdb-decimal | B · portable SQL | 604689 | 222.565 [221.559, 230.873] | 234.995 | 6 | — | — | 8.26 |
| mcar-medium | interpolate | duckdb-decimal | B · portable SQL | 604689 | 48.425 [46.462, 51.121] | 56.201 | 20 | PASS | 0.9407 | — |
| mcar-medium | downsample | duckdb-decimal | B · portable SQL | 604689 | 4.707 [4.327, 5.485] | 5.684 | 20 | PASS | — | — |
| mcar-medium | interpolate | duckdb-client | C · client-side | 604689 | 35.763 [34.259, 39.493] | 40.516 | 20 | PASS | 0.9407 | — |
| mcar-medium | load | clickhouse-double | B · portable SQL | 604689 | 122.965 [116.947, 125.459] | 126.136 | 6 | — | — | 18.83 |
| mcar-medium | interpolate | clickhouse-double | B · portable SQL | 604689 | 172.583 [160.054, 185.157] | 199.801 | 20 | PASS | 0.9407 | — |
| mcar-medium | downsample | clickhouse-double | B · portable SQL | 604689 | 6.374 [6.062, 7.268] | 7.757 | 20 | PASS | — | — |
| mcar-medium | load | clickhouse-decimal | B · portable SQL | 604689 | 123.692 [122.104, 140.494] | 140.494 | 6 | — | — | 16.00 |
| mcar-medium | interpolate | clickhouse-decimal | B · portable SQL | 604689 | 177.828 [161.836, 186.747] | 193.895 | 20 | PASS | 0.9407 | — |
| mcar-medium | downsample | clickhouse-decimal | B · portable SQL | 604689 | 5.852 [5.166, 6.400] | 6.525 | 20 | PASS | — | — |
| mcar-medium | interpolate | clickhouse-double-merge | B · portable SQL | 604689 | 245.240 [239.627, 261.722] | 267.588 | 20 | PASS | 0.9407 | — |
| mcar-medium | interpolate | clickhouse-native | A · native | 604689 | 22.052 [21.699, 22.903] | 23.722 | 20 | own answer | 0.4432 | — |
| mcar-medium | interpolate | clickhouse-client | C · client-side | 604689 | 15.327 [14.194, 16.992] | 17.450 | 20 | PASS | 0.9407 | — |
| mcar-medium | load | timescaledb-double | B · portable SQL | 604689 | 996.179 [980.640, 1008.145] | 1109.766 | 6 | — | — | 93.71 |
| mcar-medium | interpolate | timescaledb-double | B · portable SQL | 604689 | 507.145 [503.362, 510.056] | 519.850 | 20 | PASS | 0.9407 | — |
| mcar-medium | downsample | timescaledb-double | B · portable SQL | 604689 | 35.161 [34.730, 72.086] | 76.388 | 20 | FAIL (max diff 1.421e-13) | — | — |
| mcar-medium | load | timescaledb-numeric | B · portable SQL | 604689 | 899.393 [874.544, 918.305] | 952.833 | 6 | — | — | 101.89 |
| mcar-medium | interpolate | timescaledb-numeric | B · portable SQL | 604689 | 565.980 [561.194, 574.061] | 576.083 | 20 | PASS | 0.9407 | — |
| mcar-medium | downsample | timescaledb-numeric | B · portable SQL | 604689 | 47.763 [43.306, 98.277] | 98.824 | 20 | PASS | — | — |
| mcar-medium | interpolate | timescaledb-native | A · native | 604689 | 159.312 [156.591, 161.622] | 172.683 | 20 | own answer | 0.4432 | — |
| mcar-medium | interpolate | timescaledb-client | C · client-side | 604689 | 132.391 [131.443, 137.265] | 146.152 | 20 | PASS | 0.9407 | — |
| mcar-medium | load | questdb-double | B · portable SQL | 604689 | 350.754 [342.108, 361.899] | 361.899 | 6 | — | — | 83.24 |
| mcar-medium | interpolate | questdb-double | B · portable SQL | 604689 | 741.903 [706.548, 784.587] | 908.236 | 20 | PASS | 0.9407 | — |
| mcar-medium | downsample | questdb-double | B · portable SQL | 604689 | 11.498 [4.159, 17.506] | 25.489 | 20 | PASS | — | — |
| mcar-medium | load | questdb-decimal | B · portable SQL | 604689 | 358.913 [357.109, 367.757] | 367.757 | 6 | — | — | 83.24 |
| mcar-medium | interpolate | questdb-decimal | B · portable SQL | 604689 | 734.746 [715.090, 747.094] | 844.568 | 20 | PASS | 0.9407 | — |
| mcar-medium | downsample | questdb-decimal | B · portable SQL | 604689 | 14.722 [4.949, 17.542] | 21.325 | 20 | PASS | — | — |
| mcar-medium | interpolate | questdb-double-linear | B · portable SQL | 604689 | 713.888 [709.929, 759.519] | 860.159 | 20 | PASS | 0.9407 | — |
| mcar-medium | interpolate | questdb-native | A · native | 604689 | 31.842 [31.538, 32.945] | 33.350 | 20 | own answer | 0.4432 | — |
| mcar-medium | interpolate | questdb-client | C · client-side | 604689 | 51.877 [51.621, 52.561] | 52.990 | 20 | PASS | 0.9407 | — |
| mcar-medium | load | influxdb3-double | B · portable SQL | 604689 | 994.887 [907.298, 1350.346] | 1350.346 | 6 | — | — | — |
| mcar-medium | interpolate | influxdb3-double | B · portable SQL | 604689 | 334.866 [331.672, 340.763] | 350.666 | 20 | PASS | 0.9407 | — |
| mcar-medium | downsample | influxdb3-double | B · portable SQL | 604689 | 15.273 [14.593, 15.579] | 16.851 | 20 | PASS | — | — |
| mcar-medium | interpolate | influxdb3-native | A · native | 604689 | 36.262 [35.296, 38.541] | 38.952 | 20 | own answer | 0.4432 | — |
| mcar-medium | interpolate | influxdb3-client | C · client-side | 604689 | 64.187 [62.897, 65.413] | 67.250 | 20 | PASS | 0.9407 | — |
| mcar-medium | load | iotdb-double | B · portable SQL | 604689 | 514.924 [476.279, 544.563] | 544.563 | 6 | — | — | 10.51 |
| mcar-medium | interpolate | iotdb-double | B · portable SQL | 604689 | 646.550 [641.326, 654.440] | 662.157 | 20 | PASS | 0.9407 | — |
| mcar-medium | downsample | iotdb-double | B · portable SQL | 604689 | 44.921 [43.802, 45.723] | 46.446 | 20 | PASS | — | — |
| mcar-medium | interpolate | iotdb-native | A · native | 604689 | 81.136 [79.149, 82.592] | 83.435 | 20 | own answer | 0.4432 | — |
| mcar-medium | interpolate | iotdb-client | C · client-side | 604689 | 348.960 [343.595, 397.574] | 408.635 | 20 | PASS | 0.9407 | — |
| block-medium | interpolate | weftdb-memory | A · native | 481252 | 110.443 [106.326, 115.200] | 116.826 | 20 | PASS | 0.9357 | — |
| block-medium | downsample | weftdb-memory | A · native | 481252 | 5.759 [5.294, 6.143] | 6.305 | 20 | PASS | — | — |
| block-medium | load | weftdb-stored | A · native | 481252 | 4114.217 [4074.115, 4254.024] | 4254.024 | 6 | — | — | 7.13 |
| block-medium | interpolate | weftdb-stored | A · native | 481252 | 138.624 [135.420, 140.869] | 141.204 | 20 | PASS | 0.9357 | — |
| block-medium | downsample | weftdb-stored | A · native | 481252 | 14.034 [13.794, 15.510] | 16.077 | 20 | PASS | — | — |
| block-medium | load | weftdb-stored-f64 | A · native | 481252 | 4116.308 [4056.346, 4288.108] | 4288.108 | 6 | — | — | 7.13 |
| block-medium | interpolate | weftdb-stored-f64 | A · native | 481252 | 42.948 [40.685, 43.620] | 47.723 | 20 | PASS | 0.9357 | — |
| block-medium | load | weftdb-stored-bulk | A · native | 481252 | 807.529 [793.549, 822.967] | 823.335 | 6 | — | — | 6.62 |
| block-medium | interpolate | weftdb-stored-bulk | A · native | 481252 | 129.454 [127.909, 131.528] | 135.569 | 20 | PASS | 0.9357 | — |
| block-medium | downsample | weftdb-stored-bulk | A · native | 481252 | 16.770 [16.589, 17.033] | 19.673 | 20 | PASS | — | — |
| block-medium | load | duckdb-double | B · portable SQL | 481252 | 150.112 [144.663, 154.639] | 155.029 | 6 | — | — | 8.74 |
| block-medium | interpolate | duckdb-double | B · portable SQL | 481252 | 40.857 [39.893, 42.405] | 45.929 | 20 | PASS | 0.9357 | — |
| block-medium | downsample | duckdb-double | B · portable SQL | 481252 | 4.209 [3.571, 4.632] | 4.876 | 20 | PASS | — | — |
| block-medium | load | duckdb-decimal | B · portable SQL | 481252 | 184.138 [178.544, 188.190] | 189.860 | 6 | — | — | 8.20 |
| block-medium | interpolate | duckdb-decimal | B · portable SQL | 481252 | 41.810 [40.650, 44.626] | 49.106 | 20 | PASS | 0.9357 | — |
| block-medium | downsample | duckdb-decimal | B · portable SQL | 481252 | 3.669 [3.457, 3.831] | 5.074 | 20 | PASS | — | — |
| block-medium | interpolate | duckdb-client | C · client-side | 481252 | 28.691 [27.855, 29.565] | 30.253 | 20 | PASS | 0.9357 | — |
| block-medium | load | clickhouse-double | B · portable SQL | 481252 | 103.066 [96.867, 103.834] | 104.370 | 6 | — | — | 18.84 |
| block-medium | interpolate | clickhouse-double | B · portable SQL | 481252 | 141.771 [134.233, 147.764] | 152.444 | 20 | PASS | 0.9357 | — |
| block-medium | downsample | clickhouse-double | B · portable SQL | 481252 | 5.610 [4.662, 5.744] | 6.667 | 20 | PASS | — | — |
| block-medium | load | clickhouse-decimal | B · portable SQL | 481252 | 103.870 [98.744, 110.443] | 114.501 | 6 | — | — | 16.01 |
| block-medium | interpolate | clickhouse-decimal | B · portable SQL | 481252 | 146.107 [140.043, 168.118] | 169.266 | 20 | PASS | 0.9357 | — |
| block-medium | downsample | clickhouse-decimal | B · portable SQL | 481252 | 5.924 [5.095, 6.317] | 7.081 | 20 | PASS | — | — |
| block-medium | interpolate | clickhouse-double-merge | B · portable SQL | 481252 | 205.158 [197.838, 212.113] | 216.619 | 20 | PASS | 0.9357 | — |
| block-medium | interpolate | clickhouse-native | A · native | 481252 | 24.647 [23.806, 25.050] | 25.715 | 20 | own answer | 1.1935 | — |
| block-medium | interpolate | clickhouse-client | C · client-side | 481252 | 12.783 [12.586, 13.134] | 13.825 | 20 | PASS | 0.9357 | — |
| block-medium | load | timescaledb-double | B · portable SQL | 481252 | 805.109 [791.317, 867.757] | 867.757 | 6 | — | — | 93.64 |
| block-medium | interpolate | timescaledb-double | B · portable SQL | 481252 | 501.981 [498.005, 510.038] | 510.698 | 20 | PASS | 0.9357 | — |
| block-medium | downsample | timescaledb-double | B · portable SQL | 481252 | 31.556 [29.651, 57.879] | 61.805 | 20 | PASS | — | — |
| block-medium | load | timescaledb-numeric | B · portable SQL | 481252 | 707.615 [702.277, 804.120] | 804.120 | 6 | — | — | 101.73 |
| block-medium | interpolate | timescaledb-numeric | B · portable SQL | 481252 | 561.222 [555.519, 566.941] | 597.440 | 20 | PASS | 0.9357 | — |
| block-medium | downsample | timescaledb-numeric | B · portable SQL | 481252 | 78.778 [35.093, 81.432] | 84.591 | 20 | PASS | — | — |
| block-medium | interpolate | timescaledb-native | A · native | 481252 | 126.032 [122.623, 127.240] | 130.989 | 20 | own answer | 0.4756 | — |
| block-medium | interpolate | timescaledb-client | C · client-side | 481252 | 102.550 [100.973, 104.369] | 110.510 | 20 | PASS | 0.9357 | — |
| block-medium | load | questdb-double | B · portable SQL | 481252 | 293.766 [291.872, 300.230] | 300.230 | 6 | — | — | 104.58 |
| block-medium | interpolate | questdb-double | B · portable SQL | 481252 | 599.544 [578.828, 685.320] | 692.956 | 20 | PASS | 0.9357 | — |
| block-medium | downsample | questdb-double | B · portable SQL | 481252 | 11.166 [9.877, 11.914] | 13.060 | 20 | PASS | — | — |
| block-medium | load | questdb-decimal | B · portable SQL | 481252 | 301.417 [296.342, 304.859] | 304.859 | 6 | — | — | 104.58 |
| block-medium | interpolate | questdb-decimal | B · portable SQL | 481252 | 593.305 [577.755, 629.573] | 701.687 | 20 | PASS | 0.9357 | — |
| block-medium | downsample | questdb-decimal | B · portable SQL | 481252 | 11.164 [10.496, 12.574] | 14.564 | 20 | PASS | — | — |
| block-medium | interpolate | questdb-double-linear | B · portable SQL | 481252 | 583.844 [577.947, 592.829] | 723.284 | 20 | PASS | 0.9357 | — |
| block-medium | interpolate | questdb-native | A · native | 481252 | 28.000 [27.547, 28.739] | 29.319 | 20 | own answer | 0.4756 | — |
| block-medium | interpolate | questdb-client | C · client-side | 481252 | 42.156 [41.481, 43.099] | 43.653 | 20 | PASS | 0.9357 | — |
| block-medium | load | influxdb3-double | B · portable SQL | 481252 | 1001.042 [874.797, 1391.796] | 1391.796 | 6 | — | — | — |
| block-medium | interpolate | influxdb3-double | B · portable SQL | 481252 | 287.780 [281.939, 290.472] | 306.648 | 20 | PASS | 0.9357 | — |
| block-medium | downsample | influxdb3-double | B · portable SQL | 481252 | 14.230 [13.659, 14.870] | 15.532 | 20 | PASS | — | — |
| block-medium | interpolate | influxdb3-native | A · native | 481252 | 33.155 [32.492, 34.422] | 34.616 | 20 | own answer | 0.4756 | — |
| block-medium | interpolate | influxdb3-client | C · client-side | 481252 | 55.222 [54.164, 56.967] | 58.892 | 20 | PASS | 0.9357 | — |
| block-medium | load | iotdb-double | B · portable SQL | 481252 | 481.262 [430.989, 498.912] | 500.359 | 6 | — | — | 10.51 |
| block-medium | interpolate | iotdb-double | B · portable SQL | 481252 | 441.523 [438.391, 449.951] | 450.865 | 20 | PASS | 0.9357 | — |
| block-medium | downsample | iotdb-double | B · portable SQL | 481252 | 36.833 [36.240, 37.213] | 38.364 | 20 | PASS | — | — |
| block-medium | interpolate | iotdb-native | A · native | 481252 | 75.365 [74.737, 78.470] | 139.973 | 20 | own answer | 0.4756 | — |
| block-medium | interpolate | iotdb-client | C · client-side | 481252 | 314.432 [305.169, 333.840] | 342.192 | 20 | PASS | 0.9357 | — |
| uci-small | interpolate | weftdb-memory | A · native | 86400 | 18.860 [18.347, 19.134] | 19.573 | 20 | PASS | — | — |
| uci-small | downsample | weftdb-memory | A · native | 86400 | 1.662 [1.641, 1.692] | 2.119 | 20 | PASS | — | — |
| uci-small | load | weftdb-stored | A · native | 86400 | 887.359 [877.694, 938.577] | 938.577 | 6 | — | — | 2.38 |
| uci-small | interpolate | weftdb-stored | A · native | 86400 | 23.142 [22.569, 23.552] | 24.360 | 20 | PASS | — | — |
| uci-small | downsample | weftdb-stored | A · native | 86400 | 3.529 [3.028, 3.689] | 4.074 | 20 | PASS | — | — |
| uci-small | load | weftdb-stored-f64 | A · native | 86400 | 894.241 [884.074, 922.815] | 925.809 | 6 | — | — | 2.38 |
| uci-small | interpolate | weftdb-stored-f64 | A · native | 86400 | 9.044 [8.725, 9.494] | 10.626 | 20 | PASS | — | — |
| uci-small | load | weftdb-stored-bulk | A · native | 86400 | 305.982 [302.985, 317.458] | 317.458 | 6 | — | — | 1.90 |
| uci-small | interpolate | weftdb-stored-bulk | A · native | 86400 | 23.009 [22.064, 23.530] | 24.693 | 20 | PASS | — | — |
| uci-small | downsample | weftdb-stored-bulk | A · native | 86400 | 3.230 [3.114, 3.502] | 3.901 | 20 | PASS | — | — |
| uci-small | load | duckdb-double | B · portable SQL | 86400 | 46.721 [44.770, 47.916] | 48.658 | 6 | — | — | 6.21 |
| uci-small | interpolate | duckdb-double | B · portable SQL | 86400 | 24.499 [23.966, 24.992] | 28.159 | 20 | PASS | — | — |
| uci-small | downsample | duckdb-double | B · portable SQL | 86400 | 2.181 [2.041, 2.398] | 3.290 | 20 | PASS | — | — |
| uci-small | load | duckdb-decimal | B · portable SQL | 86400 | 51.980 [49.659, 54.299] | 54.299 | 6 | — | — | 6.21 |
| uci-small | interpolate | duckdb-decimal | B · portable SQL | 86400 | 26.333 [25.655, 26.617] | 30.258 | 20 | PASS | — | — |
| uci-small | downsample | duckdb-decimal | B · portable SQL | 86400 | 2.151 [1.876, 2.458] | 3.116 | 20 | PASS | — | — |
| uci-small | interpolate | duckdb-client | C · client-side | 86400 | 8.819 [8.386, 9.513] | 10.717 | 20 | PASS | — | — |
| uci-small | load | clickhouse-double | B · portable SQL | 86400 | 34.024 [32.414, 42.553] | 42.553 | 6 | — | — | 14.45 |
| uci-small | interpolate | clickhouse-double | B · portable SQL | 86400 | 79.190 [76.344, 83.086] | 86.770 | 20 | PASS | — | — |
| uci-small | downsample | clickhouse-double | B · portable SQL | 86400 | 3.536 [3.189, 3.772] | 4.441 | 20 | PASS | — | — |
| uci-small | load | clickhouse-decimal | B · portable SQL | 86400 | 37.834 [33.762, 42.123] | 42.123 | 6 | — | — | 14.20 |
| uci-small | interpolate | clickhouse-decimal | B · portable SQL | 86400 | 84.036 [78.719, 90.389] | 94.045 | 20 | PASS | — | — |
| uci-small | downsample | clickhouse-decimal | B · portable SQL | 86400 | 3.444 [3.126, 3.908] | 4.600 | 20 | PASS | — | — |
| uci-small | interpolate | clickhouse-double-merge | B · portable SQL | 86400 | 82.464 [79.229, 88.773] | 91.164 | 20 | PASS | — | — |
| uci-small | interpolate | clickhouse-native | A · native | 86400 | 16.612 [16.353, 17.183] | 17.868 | 20 | own answer | — | — |
| uci-small | interpolate | clickhouse-client | C · client-side | 86400 | 5.585 [5.430, 5.671] | 5.990 | 20 | PASS | — | — |
| uci-small | load | timescaledb-double | B · portable SQL | 86400 | 153.439 [152.779, 157.894] | 157.894 | 6 | — | — | 97.19 |
| uci-small | interpolate | timescaledb-double | B · portable SQL | 86400 | 479.300 [476.477, 480.850] | 485.491 | 20 | PASS | — | — |
| uci-small | downsample | timescaledb-double | B · portable SQL | 86400 | 11.169 [11.002, 11.372] | 12.715 | 20 | PASS | — | — |
| uci-small | load | timescaledb-numeric | B · portable SQL | 86400 | 137.927 [136.730, 139.541] | 139.541 | 6 | — | — | 97.47 |
| uci-small | interpolate | timescaledb-numeric | B · portable SQL | 86400 | 504.645 [497.656, 508.704] | 513.683 | 20 | PASS | — | — |
| uci-small | downsample | timescaledb-numeric | B · portable SQL | 86400 | 16.353 [16.162, 16.484] | 17.252 | 20 | PASS | — | — |
| uci-small | interpolate | timescaledb-native | A · native | 86400 | 85.848 [83.328, 91.148] | 93.657 | 20 | own answer | — | — |
| uci-small | interpolate | timescaledb-client | C · client-side | 86400 | 21.351 [20.682, 21.900] | 22.822 | 20 | PASS | — | — |
| uci-small | load | questdb-double | B · portable SQL | 86400 | 78.569 [72.556, 83.703] | 84.473 | 6 | — | — | 588.66 |
| uci-small | interpolate | questdb-double | B · portable SQL | 86400 | 165.913 [161.564, 173.978] | 191.606 | 20 | PASS | — | — |
| uci-small | downsample | questdb-double | B · portable SQL | 86400 | 2.607 [2.411, 2.691] | 4.605 | 20 | PASS | — | — |
| uci-small | load | questdb-decimal | B · portable SQL | 86400 | 80.138 [76.724, 91.598] | 93.400 | 6 | — | — | 588.66 |
| uci-small | interpolate | questdb-decimal | B · portable SQL | 86400 | 165.419 [158.331, 170.668] | 192.650 | 20 | PASS | — | — |
| uci-small | downsample | questdb-decimal | B · portable SQL | 86400 | 2.780 [2.741, 3.020] | 5.666 | 20 | PASS | — | — |
| uci-small | interpolate | questdb-double-linear | B · portable SQL | 86400 | 167.536 [163.892, 173.637] | 194.080 | 20 | PASS | — | — |
| uci-small | interpolate | questdb-native | A · native | 86400 | 12.922 [12.272, 13.207] | 16.339 | 20 | own answer | — | — |
| uci-small | interpolate | questdb-client | C · client-side | 86400 | 8.626 [7.962, 8.835] | 9.414 | 20 | PASS | — | — |
| uci-small | load | influxdb3-double | B · portable SQL | 86400 | 993.039 [687.728, 1091.889] | 1091.889 | 6 | — | — | — |
| uci-small | interpolate | influxdb3-double | B · portable SQL | 86400 | 1562.529 [1439.304, 1667.512] | 1735.277 | 20 | PASS | — | — |
| uci-small | downsample | influxdb3-double | B · portable SQL | 86400 | 178.681 [172.573, 189.993] | 203.816 | 20 | PASS | — | — |
| uci-small | interpolate | influxdb3-native | A · native | 86400 | 220.622 [217.376, 227.621] | 231.786 | 20 | own answer | — | — |
| uci-small | interpolate | influxdb3-client | C · client-side | 86400 | 197.326 [184.772, 210.794] | 230.762 | 20 | PASS | — | — |
| uci-small | load | iotdb-double | B · portable SQL | 86400 | 282.040 [252.941, 367.469] | 367.469 | 6 | — | — | 6.44 |
| uci-small | interpolate | iotdb-double | B · portable SQL | 86400 | 75.538 [72.502, 103.300] | 167.757 | 20 | PASS | — | — |
| uci-small | downsample | iotdb-double | B · portable SQL | 86400 | 12.880 [11.863, 13.387] | 13.568 | 20 | PASS | — | — |
| uci-small | interpolate | iotdb-native | A · native | 86400 | 60.586 [59.715, 63.010] | 65.537 | 20 | own answer | — | — |
| uci-small | interpolate | iotdb-client | C · client-side | 86400 | 46.790 [46.044, 52.201] | 66.335 | 20 | PASS | — | — |
| uci-medium | interpolate | weftdb-memory | A · native | 600863 | 137.223 [134.020, 138.183] | 140.847 | 20 | PASS | — | — |
| uci-medium | downsample | weftdb-memory | A · native | 600863 | 16.609 [16.168, 17.264] | 17.446 | 20 | PASS | — | — |
| uci-medium | load | weftdb-stored | A · native | 600863 | 5296.517 [5043.202, 5359.775] | 5384.884 | 6 | — | — | 1.89 |
| uci-medium | interpolate | weftdb-stored | A · native | 600863 | 168.009 [166.622, 170.627] | 173.702 | 20 | PASS | — | — |
| uci-medium | downsample | weftdb-stored | A · native | 600863 | 22.471 [21.391, 22.707] | 24.194 | 20 | PASS | — | — |
| uci-medium | load | weftdb-stored-f64 | A · native | 600863 | 5129.886 [5042.645, 5357.531] | 5357.531 | 6 | — | — | 1.89 |
| uci-medium | interpolate | weftdb-stored-f64 | A · native | 600863 | 53.453 [50.381, 56.713] | 58.570 | 20 | PASS | — | — |
| uci-medium | load | weftdb-stored-bulk | A · native | 600863 | 922.635 [903.270, 934.850] | 934.850 | 6 | — | — | 1.39 |
| uci-medium | interpolate | weftdb-stored-bulk | A · native | 600863 | 162.153 [158.548, 169.396] | 174.820 | 20 | PASS | — | — |
| uci-medium | downsample | weftdb-stored-bulk | A · native | 600863 | 30.869 [29.527, 31.618] | 31.835 | 20 | PASS | — | — |
| uci-medium | load | duckdb-double | B · portable SQL | 600863 | 182.071 [175.185, 187.751] | 191.431 | 6 | — | — | 3.07 |
| uci-medium | interpolate | duckdb-double | B · portable SQL | 600863 | 82.015 [79.342, 84.446] | 88.519 | 20 | PASS | — | — |
| uci-medium | downsample | duckdb-double | B · portable SQL | 600863 | 5.893 [5.596, 6.289] | 6.555 | 20 | PASS | — | — |
| uci-medium | load | duckdb-decimal | B · portable SQL | 600863 | 219.022 [216.348, 220.232] | 227.568 | 6 | — | — | 3.07 |
| uci-medium | interpolate | duckdb-decimal | B · portable SQL | 600863 | 83.996 [82.067, 85.240] | 93.989 | 20 | PASS | — | — |
| uci-medium | downsample | duckdb-decimal | B · portable SQL | 600863 | 5.859 [5.724, 6.468] | 7.363 | 20 | PASS | — | — |
| uci-medium | interpolate | duckdb-client | C · client-side | 600863 | 42.021 [40.555, 43.868] | 45.982 | 20 | PASS | — | — |
| uci-medium | load | clickhouse-double | B · portable SQL | 600863 | 111.679 [109.096, 126.007] | 126.007 | 6 | — | — | 14.05 |
| uci-medium | interpolate | clickhouse-double | B · portable SQL | 600863 | 307.616 [295.814, 315.261] | 323.835 | 20 | PASS | — | — |
| uci-medium | downsample | clickhouse-double | B · portable SQL | 600863 | 7.598 [7.527, 8.050] | 8.633 | 20 | PASS | — | — |
| uci-medium | load | clickhouse-decimal | B · portable SQL | 600863 | 119.927 [116.453, 130.576] | 130.576 | 6 | — | — | 13.83 |
| uci-medium | interpolate | clickhouse-decimal | B · portable SQL | 600863 | 311.874 [300.806, 323.548] | 329.657 | 20 | PASS | — | — |
| uci-medium | downsample | clickhouse-decimal | B · portable SQL | 600863 | 8.045 [7.962, 8.988] | 9.015 | 20 | PASS | — | — |
| uci-medium | interpolate | clickhouse-double-merge | B · portable SQL | 600863 | 299.502 [295.464, 309.098] | 311.098 | 20 | PASS | — | — |
| uci-medium | interpolate | clickhouse-native | A · native | 600863 | 84.868 [83.493, 87.289] | 88.918 | 20 | own answer | — | — |
| uci-medium | interpolate | clickhouse-client | C · client-side | 600863 | 20.093 [19.769, 21.052] | 21.388 | 20 | PASS | — | — |
| uci-medium | load | timescaledb-double | B · portable SQL | 600863 | 991.661 [972.257, 1006.343] | 1006.343 | 6 | — | — | 93.08 |
| uci-medium | interpolate | timescaledb-double | B · portable SQL | 600863 | 3326.176 [3302.281, 3352.172] | 3376.507 | 20 | PASS | — | — |
| uci-medium | downsample | timescaledb-double | B · portable SQL | 600863 | 79.600 [78.345, 80.757] | 81.209 | 20 | PASS | — | — |
| uci-medium | load | timescaledb-numeric | B · portable SQL | 600863 | 858.025 [852.423, 865.965] | 888.196 | 6 | — | — | 93.12 |
| uci-medium | interpolate | timescaledb-numeric | B · portable SQL | 600863 | 3469.782 [3445.106, 3497.320] | 3521.880 | 20 | PASS | — | — |
| uci-medium | downsample | timescaledb-numeric | B · portable SQL | 600863 | 112.496 [110.368, 114.094] | 115.266 | 20 | PASS | — | — |
| uci-medium | interpolate | timescaledb-native | A · native | 600863 | 697.737 [685.471, 715.389] | 738.130 | 20 | own answer | — | — |
| uci-medium | interpolate | timescaledb-client | C · client-side | 600863 | 134.988 [133.574, 136.163] | 146.204 | 20 | PASS | — | — |
| uci-medium | load | questdb-double | B · portable SQL | 600863 | 349.734 [346.148, 355.736] | 363.103 | 6 | — | — | 105.48 |
| uci-medium | interpolate | questdb-double | B · portable SQL | 600863 | 906.736 [891.876, 1008.708] | 1077.197 | 20 | PASS | — | — |
| uci-medium | downsample | questdb-double | B · portable SQL | 600863 | 13.652 [13.110, 28.061] | 76.361 | 20 | PASS | — | — |
| uci-medium | load | questdb-decimal | B · portable SQL | 600863 | 367.129 [357.270, 373.050] | 374.677 | 6 | — | — | 105.48 |
| uci-medium | interpolate | questdb-decimal | B · portable SQL | 600863 | 910.426 [881.823, 931.898] | 1083.205 | 20 | PASS | — | — |
| uci-medium | downsample | questdb-decimal | B · portable SQL | 600863 | 13.632 [12.737, 29.442] | 52.347 | 20 | PASS | — | — |
| uci-medium | interpolate | questdb-double-linear | B · portable SQL | 600863 | 908.965 [875.140, 928.271] | 1036.450 | 20 | PASS | — | — |
| uci-medium | interpolate | questdb-native | A · native | 600863 | 99.885 [94.542, 106.324] | 115.445 | 20 | own answer | — | — |
| uci-medium | interpolate | questdb-client | C · client-side | 600863 | 58.920 [57.475, 59.520] | 64.599 | 20 | PASS | — | — |
| uci-medium | load | influxdb3-double | B · portable SQL | 600863 | 1014.495 [988.308, 1754.797] | 1754.797 | 6 | — | — | — |
| uci-medium | interpolate | influxdb3-double | B · portable SQL | 600863 | 12184.948 [11634.071, 13068.641] | 13558.799 | 20 | PASS | — | — |
| uci-medium | downsample | influxdb3-double | B · portable SQL | 600863 | 1616.750 [1554.916, 1665.431] | 1720.368 | 20 | PASS | — | — |
| uci-medium | interpolate | influxdb3-native | A · native | 600863 | 1762.081 [1713.672, 1811.463] | 1894.466 | 20 | own answer | — | — |
| uci-medium | interpolate | influxdb3-client | C · client-side | 600863 | 1578.967 [1552.450, 1645.501] | 1722.055 | 20 | PASS | — | — |
| uci-medium | load | iotdb-double | B · portable SQL | 600863 | 992.226 [922.106, 1161.799] | 1161.799 | 6 | — | — | 5.41 |
| uci-medium | interpolate | iotdb-double | B · portable SQL | 600863 | 904.216 [891.862, 984.216] | 1048.165 | 20 | PASS | — | — |
| uci-medium | downsample | iotdb-double | B · portable SQL | 600863 | 54.859 [54.332, 55.371] | 56.514 | 20 | PASS | — | — |
| uci-medium | interpolate | iotdb-native | A · native | 600863 | 465.488 [454.175, 509.679] | 513.445 | 20 | own answer | — | — |
| uci-medium | interpolate | iotdb-client | C · client-side | 600863 | 371.475 [358.665, 392.542] | 393.325 | 20 | PASS | — | — |
| tsbs-small | interpolate | weftdb-memory | A · native | 8641 | 5.039 [4.744, 5.502] | 5.773 | 20 | PASS | — | — |
| tsbs-small | downsample | weftdb-memory | A · native | 8641 | 0.929 [0.909, 0.943] | 1.330 | 20 | PASS | — | — |
| tsbs-small | load | weftdb-stored | A · native | 8641 | 256.239 [251.721, 256.877] | 268.816 | 6 | — | — | 9.62 |
| tsbs-small | interpolate | weftdb-stored | A · native | 8641 | 5.988 [5.883, 6.124] | 7.351 | 20 | PASS | — | — |
| tsbs-small | downsample | weftdb-stored | A · native | 8641 | 1.546 [1.441, 1.869] | 2.398 | 20 | PASS | — | — |
| tsbs-small | load | weftdb-stored-f64 | A · native | 8641 | 267.731 [254.247, 274.228] | 282.747 | 6 | — | — | 9.62 |
| tsbs-small | interpolate | weftdb-stored-f64 | A · native | 8641 | 2.177 [1.780, 2.587] | 3.246 | 20 | PASS | — | — |
| tsbs-small | load | weftdb-stored-bulk | A · native | 8641 | 199.962 [196.078, 201.646] | 202.664 | 6 | — | — | 9.20 |
| tsbs-small | interpolate | weftdb-stored-bulk | A · native | 8641 | 5.568 [5.379, 6.251] | 7.174 | 20 | PASS | — | — |
| tsbs-small | downsample | weftdb-stored-bulk | A · native | 8641 | 1.496 [1.464, 1.556] | 2.157 | 20 | PASS | — | — |
| tsbs-small | load | duckdb-double | B · portable SQL | 8641 | 21.036 [20.745, 23.835] | 23.835 | 6 | — | — | 62.10 |
| tsbs-small | interpolate | duckdb-double | B · portable SQL | 8641 | 17.739 [17.149, 18.765] | 20.192 | 20 | PASS | — | — |
| tsbs-small | downsample | duckdb-double | B · portable SQL | 8641 | 1.117 [1.036, 1.206] | 2.328 | 20 | PASS | — | — |
| tsbs-small | load | duckdb-decimal | B · portable SQL | 8641 | 22.564 [22.413, 23.131] | 23.574 | 6 | — | — | 62.10 |
| tsbs-small | interpolate | duckdb-decimal | B · portable SQL | 8641 | 19.236 [18.266, 19.996] | 21.237 | 20 | PASS | — | — |
| tsbs-small | downsample | duckdb-decimal | B · portable SQL | 8641 | 1.254 [1.175, 1.285] | 2.148 | 20 | PASS | — | — |
| tsbs-small | interpolate | duckdb-client | C · client-side | 8641 | 2.295 [2.257, 2.462] | 3.860 | 20 | PASS | — | — |
| tsbs-small | load | clickhouse-double | B · portable SQL | 8641 | 20.702 [19.315, 21.425] | 83.325 | 6 | — | — | 15.14 |
| tsbs-small | interpolate | clickhouse-double | B · portable SQL | 8641 | 38.591 [36.051, 41.912] | 46.363 | 20 | PASS | — | — |
| tsbs-small | downsample | clickhouse-double | B · portable SQL | 8641 | 1.379 [1.302, 1.456] | 2.367 | 20 | PASS | — | — |
| tsbs-small | load | clickhouse-decimal | B · portable SQL | 8641 | 20.248 [19.806, 22.108] | 22.986 | 6 | — | — | 15.13 |
| tsbs-small | interpolate | clickhouse-decimal | B · portable SQL | 8641 | 37.086 [35.592, 38.347] | 40.313 | 20 | PASS | — | — |
| tsbs-small | downsample | clickhouse-decimal | B · portable SQL | 8641 | 1.369 [1.310, 1.625] | 2.376 | 20 | PASS | — | — |
| tsbs-small | interpolate | clickhouse-double-merge | B · portable SQL | 8641 | 35.896 [34.080, 37.134] | 37.523 | 20 | PASS | — | — |
| tsbs-small | interpolate | clickhouse-native | A · native | 8641 | 28.291 [27.683, 28.556] | 29.979 | 20 | own answer | — | — |
| tsbs-small | interpolate | clickhouse-client | C · client-side | 8641 | 2.030 [1.993, 2.181] | 2.714 | 20 | PASS | — | — |
| tsbs-small | load | timescaledb-double | B · portable SQL | 8641 | 28.428 [28.069, 30.244] | 30.244 | 6 | — | — | 102.39 |
| tsbs-small | interpolate | timescaledb-double | B · portable SQL | 8641 | 464.880 [461.261, 468.685] | 471.291 | 20 | PASS | — | — |
| tsbs-small | downsample | timescaledb-double | B · portable SQL | 8641 | 2.243 [2.166, 2.281] | 3.492 | 20 | PASS | — | — |
| tsbs-small | load | timescaledb-numeric | B · portable SQL | 8641 | 30.047 [29.867, 30.413] | 33.282 | 6 | — | — | 105.23 |
| tsbs-small | interpolate | timescaledb-numeric | B · portable SQL | 8641 | 474.372 [472.143, 483.366] | 486.284 | 20 | PASS | — | — |
| tsbs-small | downsample | timescaledb-numeric | B · portable SQL | 8641 | 3.800 [3.746, 3.864] | 5.070 | 20 | PASS | — | — |
| tsbs-small | interpolate | timescaledb-native | A · native | 8641 | 18.731 [18.449, 19.678] | 20.149 | 20 | own answer | — | — |
| tsbs-small | interpolate | timescaledb-client | C · client-side | 8641 | 2.977 [2.905, 3.145] | 4.035 | 20 | PASS | — | — |
| tsbs-small | load | questdb-double | B · portable SQL | 8641 | 49.039 [47.562, 52.625] | 52.625 | 6 | — | — | 5824.75 |
| tsbs-small | interpolate | questdb-double | B · portable SQL | 8641 | 40.209 [39.367, 41.128] | 46.567 | 20 | PASS | — | — |
| tsbs-small | downsample | questdb-double | B · portable SQL | 8641 | 0.585 [0.525, 0.695] | 2.604 | 20 | PASS | — | — |
| tsbs-small | load | questdb-decimal | B · portable SQL | 8641 | 49.321 [48.302, 51.901] | 51.901 | 6 | — | — | 5824.75 |
| tsbs-small | interpolate | questdb-decimal | B · portable SQL | 8641 | 40.881 [39.941, 41.824] | 47.105 | 20 | PASS | — | — |
| tsbs-small | downsample | questdb-decimal | B · portable SQL | 8641 | 0.692 [0.615, 0.725] | 2.573 | 20 | PASS | — | — |
| tsbs-small | interpolate | questdb-double-linear | B · portable SQL | 8641 | 42.334 [40.264, 43.596] | 47.666 | 20 | PASS | — | — |
| tsbs-small | interpolate | questdb-native | A · native | 8641 | 12.509 [11.844, 13.190] | 15.593 | 20 | own answer | — | — |
| tsbs-small | interpolate | questdb-client | C · client-side | 8641 | 1.818 [1.735, 1.857] | 2.535 | 20 | PASS | — | — |
| tsbs-small | load | influxdb3-double | B · portable SQL | 8641 | 820.278 [329.530, 999.770] | 999.770 | 6 | — | — | — |
| tsbs-small | interpolate | influxdb3-double | B · portable SQL | 8641 | 79.196 [77.436, 81.832] | 94.511 | 20 | PASS | — | — |
| tsbs-small | downsample | influxdb3-double | B · portable SQL | 8641 | 6.653 [6.238, 7.132] | 7.518 | 20 | PASS | — | — |
| tsbs-small | interpolate | influxdb3-native | A · native | 8641 | 19.660 [19.456, 20.280] | 21.425 | 20 | own answer | — | — |
| tsbs-small | interpolate | influxdb3-client | C · client-side | 8641 | 6.776 [6.492, 7.013] | 8.073 | 20 | PASS | — | — |
| tsbs-small | load | iotdb-double | B · portable SQL | 8641 | 231.234 [201.032, 249.696] | 252.978 | 6 | — | — | 0.82 |
| tsbs-small | interpolate | iotdb-double | B · portable SQL | 8641 | 52.289 [51.452, 52.832] | 53.925 | 20 | PASS | — | — |
| tsbs-small | downsample | iotdb-double | B · portable SQL | 8641 | 5.554 [5.408, 5.828] | 7.130 | 20 | PASS | — | — |
| tsbs-small | interpolate | iotdb-native | A · native | 8641 | 46.526 [46.249, 47.363] | 47.780 | 20 | own answer | — | — |
| tsbs-small | interpolate | iotdb-client | C · client-side | 8641 | 8.404 [8.039, 9.228] | 11.166 | 20 | PASS | — | — |
| tsbs-medium | interpolate | weftdb-memory | A · native | 60481 | 23.340 [23.152, 23.700] | 24.993 | 20 | PASS | — | — |
| tsbs-medium | downsample | weftdb-memory | A · native | 60481 | 6.652 [6.356, 6.999] | 7.764 | 20 | PASS | — | — |
| tsbs-medium | load | weftdb-stored | A · native | 60481 | 618.212 [616.102, 634.663] | 642.046 | 6 | — | — | 5.18 |
| tsbs-medium | interpolate | weftdb-stored | A · native | 60481 | 27.900 [26.174, 28.711] | 29.849 | 20 | PASS | — | — |
| tsbs-medium | downsample | weftdb-stored | A · native | 60481 | 9.320 [8.441, 11.276] | 11.704 | 20 | PASS | — | — |
| tsbs-medium | load | weftdb-stored-f64 | A · native | 60481 | 636.215 [622.742, 645.643] | 645.879 | 6 | — | — | 5.19 |
| tsbs-medium | interpolate | weftdb-stored-f64 | A · native | 60481 | 10.258 [9.589, 10.924] | 11.599 | 20 | PASS | — | — |
| tsbs-medium | load | weftdb-stored-bulk | A · native | 60481 | 211.896 [210.626, 215.850] | 215.850 | 6 | — | — | 4.74 |
| tsbs-medium | interpolate | weftdb-stored-bulk | A · native | 60481 | 27.226 [26.787, 28.657] | 29.774 | 20 | PASS | — | — |
| tsbs-medium | downsample | weftdb-stored-bulk | A · native | 60481 | 10.179 [9.973, 10.318] | 10.448 | 20 | PASS | — | — |
| tsbs-medium | load | duckdb-double | B · portable SQL | 60481 | 33.787 [33.302, 36.648] | 37.931 | 6 | — | — | 8.87 |
| tsbs-medium | interpolate | duckdb-double | B · portable SQL | 60481 | 62.302 [59.693, 63.598] | 66.593 | 20 | PASS | — | — |
| tsbs-medium | downsample | duckdb-double | B · portable SQL | 60481 | 3.051 [2.914, 3.541] | 4.114 | 20 | PASS | — | — |
| tsbs-medium | load | duckdb-decimal | B · portable SQL | 60481 | 42.216 [40.046, 43.552] | 46.382 | 6 | — | — | 8.87 |
| tsbs-medium | interpolate | duckdb-decimal | B · portable SQL | 60481 | 63.232 [60.661, 68.824] | 72.184 | 20 | PASS | — | — |
| tsbs-medium | downsample | duckdb-decimal | B · portable SQL | 60481 | 3.374 [3.138, 3.660] | 4.098 | 20 | PASS | — | — |
| tsbs-medium | interpolate | duckdb-client | C · client-side | 60481 | 13.429 [13.094, 13.831] | 15.558 | 20 | PASS | — | — |
| tsbs-medium | load | clickhouse-double | B · portable SQL | 60481 | 27.291 [25.533, 41.155] | 79.843 | 6 | — | — | 13.55 |
| tsbs-medium | interpolate | clickhouse-double | B · portable SQL | 60481 | 187.035 [185.489, 189.217] | 199.375 | 20 | PASS | — | — |
| tsbs-medium | downsample | clickhouse-double | B · portable SQL | 60481 | 3.344 [3.093, 3.554] | 4.703 | 20 | PASS | — | — |
| tsbs-medium | load | clickhouse-decimal | B · portable SQL | 60481 | 29.060 [27.507, 33.474] | 33.474 | 6 | — | — | 13.54 |
| tsbs-medium | interpolate | clickhouse-decimal | B · portable SQL | 60481 | 196.024 [191.983, 202.305] | 209.257 | 20 | PASS | — | — |
| tsbs-medium | downsample | clickhouse-decimal | B · portable SQL | 60481 | 3.250 [2.878, 3.611] | 4.437 | 20 | PASS | — | — |
| tsbs-medium | interpolate | clickhouse-double-merge | B · portable SQL | 60481 | 119.971 [116.541, 122.820] | 124.739 | 20 | PASS | — | — |
| tsbs-medium | interpolate | clickhouse-native | A · native | 60481 | 178.578 [176.651, 181.065] | 185.406 | 20 | own answer | — | — |
| tsbs-medium | interpolate | clickhouse-client | C · client-side | 60481 | 11.199 [10.692, 11.520] | 11.683 | 20 | PASS | — | — |
| tsbs-medium | load | timescaledb-double | B · portable SQL | 60481 | 100.977 [96.691, 101.213] | 102.813 | 6 | — | — | 100.23 |
| tsbs-medium | interpolate | timescaledb-double | B · portable SQL | 60481 | 3184.885 [3173.014, 3251.057] | 3262.144 | 20 | PASS | — | — |
| tsbs-medium | downsample | timescaledb-double | B · portable SQL | 60481 | 16.135 [15.554, 16.776] | 17.791 | 20 | PASS | — | — |
| tsbs-medium | load | timescaledb-numeric | B · portable SQL | 60481 | 102.567 [101.360, 108.167] | 108.167 | 6 | — | — | 100.64 |
| tsbs-medium | interpolate | timescaledb-numeric | B · portable SQL | 60481 | 3297.711 [3275.404, 3318.156] | 3340.394 | 20 | PASS | — | — |
| tsbs-medium | downsample | timescaledb-numeric | B · portable SQL | 60481 | 28.884 [26.657, 29.593] | 29.722 | 20 | PASS | — | — |
| tsbs-medium | interpolate | timescaledb-native | A · native | 60481 | 132.038 [128.028, 136.737] | 145.150 | 20 | own answer | — | — |
| tsbs-medium | interpolate | timescaledb-client | C · client-side | 60481 | 21.239 [20.493, 22.186] | 24.018 | 20 | PASS | — | — |
| tsbs-medium | load | questdb-double | B · portable SQL | 60481 | 65.075 [64.138, 70.065] | 70.619 | 6 | — | — | 832.19 |
| tsbs-medium | interpolate | questdb-double | B · portable SQL | 60481 | 340.574 [336.514, 348.902] | 352.278 | 20 | PASS | — | — |
| tsbs-medium | downsample | questdb-double | B · portable SQL | 60481 | 12.243 [10.970, 12.620] | 14.380 | 20 | PASS | — | — |
| tsbs-medium | load | questdb-decimal | B · portable SQL | 60481 | 70.778 [69.892, 72.415] | 77.452 | 6 | — | — | 832.19 |
| tsbs-medium | interpolate | questdb-decimal | B · portable SQL | 60481 | 344.503 [339.071, 348.604] | 353.693 | 20 | PASS | — | — |
| tsbs-medium | downsample | questdb-decimal | B · portable SQL | 60481 | 12.385 [11.109, 13.174] | 24.080 | 20 | PASS | — | — |
| tsbs-medium | interpolate | questdb-double-linear | B · portable SQL | 60481 | 342.917 [338.124, 346.505] | 351.371 | 20 | PASS | — | — |
| tsbs-medium | interpolate | questdb-native | A · native | 60481 | 96.026 [94.453, 97.417] | 104.135 | 20 | own answer | — | — |
| tsbs-medium | interpolate | questdb-client | C · client-side | 60481 | 12.318 [12.103, 12.530] | 13.207 | 20 | PASS | — | — |
| tsbs-medium | load | influxdb3-double | B · portable SQL | 60481 | 826.136 [689.989, 1003.309] | 1168.415 | 6 | — | — | — |
| tsbs-medium | interpolate | influxdb3-double | B · portable SQL | 60481 | 439.650 [434.536, 457.379] | 465.910 | 20 | PASS | — | — |
| tsbs-medium | downsample | influxdb3-double | B · portable SQL | 60481 | 26.036 [24.802, 27.896] | 29.538 | 20 | PASS | — | — |
| tsbs-medium | interpolate | influxdb3-native | A · native | 60481 | 97.431 [95.202, 97.861] | 101.367 | 20 | own answer | — | — |
| tsbs-medium | interpolate | influxdb3-client | C · client-side | 60481 | 31.514 [30.544, 34.841] | 37.638 | 20 | PASS | — | — |
| tsbs-medium | load | iotdb-double | B · portable SQL | 60481 | 235.469 [196.082, 244.804] | 246.479 | 6 | — | — | 0.83 |
| tsbs-medium | interpolate | iotdb-double | B · portable SQL | 60481 | 349.864 [347.780, 356.095] | 372.611 | 20 | PASS | — | — |
| tsbs-medium | downsample | iotdb-double | B · portable SQL | 60481 | 18.290 [17.597, 18.772] | 19.032 | 20 | PASS | — | — |
| tsbs-medium | interpolate | iotdb-native | A · native | 60481 | 386.432 [383.575, 390.644] | 391.667 | 20 | own answer | — | — |
| tsbs-medium | interpolate | iotdb-client | C · client-side | 60481 | 37.099 [35.952, 38.163] | 39.271 | 20 | PASS | — | — |

## Interpolation classes

Each engine is asked for the grid three ways where it can be (fair protocol 1.2): **A · native**, the engine's own interpolation or fill; **B · portable SQL**, the linear reconstruction written in its SQL; **C · client-side**, every knot fetched into this Rust client and interpolated there, end to end. B and C answer the question WeftDB answers, so they are gated against its grid and timed against it. A native fill that computes something else is gated only on the grid's shape, and is shown below with how far its answer lies from the reconstruction instead of being raced against it. DuckDB has no native interpolation, so it has no class A arm.

| dataset | phase | native fill | p50 [CI] | unfilled instants | max diff from the line | RMSE vs truth |
| --- | --- | --- | --- | --- | --- | --- |
| mcar-small | interpolate | clickhouse-native | 21.003 [20.173, 21.151] | 0 | 3.826e0 | 1.0586 |
| mcar-small | interpolate | timescaledb-native | 73.610 [71.243, 76.353] | 1 | 3.826e0 | 1.0031 |
| mcar-small | interpolate | questdb-native | 14.672 [14.334, 15.206] | 1 | 3.826e0 | 1.0031 |
| mcar-small | interpolate | influxdb3-native | 27.793 [26.996, 29.167] | 1 | 3.826e0 | 1.0031 |
| mcar-small | interpolate | iotdb-native | 68.810 [62.391, 171.542] | 1 | 3.826e0 | 1.0031 |
| block-small | interpolate | clickhouse-native | 23.481 [23.046, 23.708] | 0 | 9.654e0 | 1.4646 |
| block-small | interpolate | timescaledb-native | 58.735 [55.123, 62.025] | 2 | 3.806e0 | 0.9623 |
| block-small | interpolate | questdb-native | 13.877 [13.804, 14.135] | 2 | 3.806e0 | 0.9623 |
| block-small | interpolate | influxdb3-native | 23.915 [23.376, 25.148] | 2 | 3.806e0 | 0.9623 |
| block-small | interpolate | iotdb-native | 64.552 [59.714, 128.607] | 2 | 3.806e0 | 0.9623 |
| mcar-medium | interpolate | clickhouse-native | 22.052 [21.699, 22.903] | 0 | 3.204e0 | 0.4432 |
| mcar-medium | interpolate | timescaledb-native | 159.312 [156.591, 161.622] | 1 | 3.204e0 | 0.4432 |
| mcar-medium | interpolate | questdb-native | 31.842 [31.538, 32.945] | 1 | 3.204e0 | 0.4432 |
| mcar-medium | interpolate | influxdb3-native | 36.262 [35.296, 38.541] | 1 | 3.204e0 | 0.4432 |
| mcar-medium | interpolate | iotdb-native | 81.136 [79.149, 82.592] | 1 | 3.204e0 | 0.4432 |
| block-medium | interpolate | clickhouse-native | 24.647 [23.806, 25.050] | 0 | 1.101e1 | 1.1935 |
| block-medium | interpolate | timescaledb-native | 126.032 [122.623, 127.240] | 1 | 2.971e0 | 0.4756 |
| block-medium | interpolate | questdb-native | 28.000 [27.547, 28.739] | 1 | 2.971e0 | 0.4756 |
| block-medium | interpolate | influxdb3-native | 33.155 [32.492, 34.422] | 1 | 2.971e0 | 0.4756 |
| block-medium | interpolate | iotdb-native | 75.365 [74.737, 78.470] | 1 | 2.971e0 | 0.4756 |
| uci-small | interpolate | clickhouse-native | 16.612 [16.353, 17.183] | 0 | 3.987e-1 | — |
| uci-small | interpolate | timescaledb-native | 85.848 [83.328, 91.148] | 0 | 5.829e-16 | — |
| uci-small | interpolate | questdb-native | 12.922 [12.272, 13.207] | 0 | 5.829e-16 | — |
| uci-small | interpolate | influxdb3-native | 220.622 [217.376, 227.621] | 0 | 5.829e-16 | — |
| uci-small | interpolate | iotdb-native | 60.586 [59.715, 63.010] | 0 | 5.829e-16 | — |
| uci-medium | interpolate | clickhouse-native | 84.868 [83.493, 87.289] | 0 | 1.789e0 | — |
| uci-medium | interpolate | timescaledb-native | 697.737 [685.471, 715.389] | 0 | 8.882e-16 | — |
| uci-medium | interpolate | questdb-native | 99.885 [94.542, 106.324] | 0 | 8.882e-16 | — |
| uci-medium | interpolate | influxdb3-native | 1762.081 [1713.672, 1811.463] | 0 | 8.882e-16 | — |
| uci-medium | interpolate | iotdb-native | 465.488 [454.175, 509.679] | 0 | 8.882e-16 | — |
| tsbs-small | interpolate | clickhouse-native | 28.291 [27.683, 28.556] | 0 | 3.600e0 | — |
| tsbs-small | interpolate | timescaledb-native | 18.731 [18.449, 19.678] | 0 | 1.421e-14 | — |
| tsbs-small | interpolate | questdb-native | 12.509 [11.844, 13.190] | 0 | 1.421e-14 | — |
| tsbs-small | interpolate | influxdb3-native | 19.660 [19.456, 20.280] | 0 | 1.421e-14 | — |
| tsbs-small | interpolate | iotdb-native | 46.526 [46.249, 47.363] | 0 | 1.421e-14 | — |
| tsbs-medium | interpolate | clickhouse-native | 178.578 [176.651, 181.065] | 0 | 3.600e0 | — |
| tsbs-medium | interpolate | timescaledb-native | 132.038 [128.028, 136.737] | 0 | 1.421e-14 | — |
| tsbs-medium | interpolate | questdb-native | 96.026 [94.453, 97.417] | 0 | 1.421e-14 | — |
| tsbs-medium | interpolate | influxdb3-native | 97.431 [95.202, 97.861] | 0 | 1.421e-14 | — |
| tsbs-medium | interpolate | iotdb-native | 386.432 [383.575, 390.644] | 0 | 1.421e-14 | — |

- `clickhouse-native` computes the average of the knots in each grid-step bucket, carried forward unchanged across empty buckets (last observation carried forward, not a line), and 0 (the column's default) before the first knot.
- `influxdb3-native` computes the average of the knots in each grid-step bucket, labelled with the bucket's start, and the line between the averages of the nearest filled buckets across empty ones; nothing before the first knot's bucket or after the last's.
- `iotdb-native` computes the average of the knots in each grid-step bucket, labelled with the bucket's start, and the line between the averages of the nearest filled buckets across empty ones; nothing before the first knot's bucket or after the last's.
- `questdb-native` computes the average of the knots in each grid-step bucket, labelled with the bucket's start, and the line between the averages of the nearest filled buckets across empty ones.
- `timescaledb-native` computes the average of the knots in each grid-step bucket, labelled with the bucket's start, and the line between the averages of the nearest filled buckets across empty ones; nothing before the first knot's bucket or after the last's.

## Honesty

Generated from the results above. A WeftDB arm wins or loses only when its p50 interval does not overlap the fastest other engine's; overlapping intervals are within noise.

### Correctness failures

- `mcar-medium` downsample `timescaledb-double` disagrees with the in-memory reference: Some(Correctness { expected_points: 1440, actual_points: 1440, values_finite: true, max_abs_diff_vs_reference: 1.4210854715202006e-13, tolerance: 9.1980871e-8, passed: false, unfilled_points: 0, answer_sha256: None }).

### Where WeftDB wins

- `mcar-small` interpolate vs B · portable SQL: `weftdb-memory` p50 21.038 ms vs `duckdb-double` 24.475 ms (weftdb-memory 1.2× faster).
- `mcar-small` downsample vs B · portable SQL: `weftdb-memory` p50 1.676 ms vs `duckdb-double` 2.080 ms (weftdb-memory 1.2× faster).
- `mcar-small` interpolate vs B · portable SQL: `weftdb-stored-f64` p50 8.910 ms vs `duckdb-double` 24.475 ms (weftdb-stored-f64 2.7× faster).
- `block-small` interpolate vs B · portable SQL: `weftdb-memory` p50 18.856 ms vs `duckdb-double` 23.080 ms (weftdb-memory 1.2× faster).
- `block-small` downsample vs B · portable SQL: `weftdb-memory` p50 1.402 ms vs `duckdb-decimal` 1.854 ms (weftdb-memory 1.3× faster).
- `block-small` interpolate vs B · portable SQL: `weftdb-stored-f64` p50 8.324 ms vs `duckdb-double` 23.080 ms (weftdb-stored-f64 2.8× faster).
- `uci-small` interpolate vs B · portable SQL: `weftdb-memory` p50 18.860 ms vs `duckdb-double` 24.499 ms (weftdb-memory 1.3× faster).
- `uci-small` downsample vs B · portable SQL: `weftdb-memory` p50 1.662 ms vs `duckdb-decimal` 2.151 ms (weftdb-memory 1.3× faster).
- `uci-small` interpolate vs B · portable SQL: `weftdb-stored` p50 23.142 ms vs `duckdb-double` 24.499 ms (weftdb-stored 1.1× faster).
- `uci-small` interpolate vs B · portable SQL: `weftdb-stored-f64` p50 9.044 ms vs `duckdb-double` 24.499 ms (weftdb-stored-f64 2.7× faster).
- `uci-small` interpolate vs B · portable SQL: `weftdb-stored-bulk` p50 23.009 ms vs `duckdb-double` 24.499 ms (weftdb-stored-bulk 1.1× faster).
- `uci-medium` interpolate vs B · portable SQL: `weftdb-stored-f64` p50 53.453 ms vs `duckdb-double` 82.015 ms (weftdb-stored-f64 1.5× faster).
- `tsbs-small` interpolate vs B · portable SQL: `weftdb-memory` p50 5.039 ms vs `duckdb-double` 17.739 ms (weftdb-memory 3.5× faster).
- `tsbs-small` interpolate vs B · portable SQL: `weftdb-stored` p50 5.988 ms vs `duckdb-double` 17.739 ms (weftdb-stored 3.0× faster).
- `tsbs-small` interpolate vs B · portable SQL: `weftdb-stored-f64` p50 2.177 ms vs `duckdb-double` 17.739 ms (weftdb-stored-f64 8.2× faster).
- `tsbs-small` interpolate vs B · portable SQL: `weftdb-stored-bulk` p50 5.568 ms vs `duckdb-double` 17.739 ms (weftdb-stored-bulk 3.2× faster).
- `tsbs-medium` interpolate vs B · portable SQL: `weftdb-memory` p50 23.340 ms vs `duckdb-double` 62.302 ms (weftdb-memory 2.7× faster).
- `tsbs-medium` interpolate vs B · portable SQL: `weftdb-stored` p50 27.900 ms vs `duckdb-double` 62.302 ms (weftdb-stored 2.2× faster).
- `tsbs-medium` interpolate vs B · portable SQL: `weftdb-stored-f64` p50 10.258 ms vs `duckdb-double` 62.302 ms (weftdb-stored-f64 6.1× faster).
- `tsbs-medium` interpolate vs B · portable SQL: `weftdb-stored-bulk` p50 27.226 ms vs `duckdb-double` 62.302 ms (weftdb-stored-bulk 2.3× faster).

### Where WeftDB loses

- `mcar-small` interpolate vs C · client-side: `weftdb-memory` p50 21.038 ms vs `clickhouse-client` 5.534 ms (clickhouse-client 3.8× faster).
- `mcar-small` load vs B · portable SQL: `weftdb-stored` p50 910.332 ms vs `clickhouse-double` 34.519 ms (clickhouse-double 26.4× faster).
- `mcar-small` interpolate vs B · portable SQL: `weftdb-stored` p50 26.247 ms vs `duckdb-double` 24.475 ms (duckdb-double 1.1× faster).
- `mcar-small` interpolate vs C · client-side: `weftdb-stored` p50 26.247 ms vs `clickhouse-client` 5.534 ms (clickhouse-client 4.7× faster).
- `mcar-small` downsample vs B · portable SQL: `weftdb-stored` p50 4.019 ms vs `duckdb-double` 2.080 ms (duckdb-double 1.9× faster).
- `mcar-small` load vs B · portable SQL: `weftdb-stored-f64` p50 897.594 ms vs `clickhouse-double` 34.519 ms (clickhouse-double 26.0× faster).
- `mcar-small` interpolate vs C · client-side: `weftdb-stored-f64` p50 8.910 ms vs `clickhouse-client` 5.534 ms (clickhouse-client 1.6× faster).
- `mcar-small` load vs B · portable SQL: `weftdb-stored-bulk` p50 305.404 ms vs `clickhouse-double` 34.519 ms (clickhouse-double 8.8× faster).
- `mcar-small` interpolate vs B · portable SQL: `weftdb-stored-bulk` p50 27.742 ms vs `duckdb-double` 24.475 ms (duckdb-double 1.1× faster).
- `mcar-small` interpolate vs C · client-side: `weftdb-stored-bulk` p50 27.742 ms vs `clickhouse-client` 5.534 ms (clickhouse-client 5.0× faster).
- `mcar-small` downsample vs B · portable SQL: `weftdb-stored-bulk` p50 4.048 ms vs `duckdb-double` 2.080 ms (duckdb-double 1.9× faster).
- `block-small` interpolate vs C · client-side: `weftdb-memory` p50 18.856 ms vs `clickhouse-client` 5.050 ms (clickhouse-client 3.7× faster).
- `block-small` load vs B · portable SQL: `weftdb-stored` p50 765.650 ms vs `clickhouse-decimal` 32.899 ms (clickhouse-decimal 23.3× faster).
- `block-small` interpolate vs C · client-side: `weftdb-stored` p50 22.706 ms vs `clickhouse-client` 5.050 ms (clickhouse-client 4.5× faster).
- `block-small` downsample vs B · portable SQL: `weftdb-stored` p50 3.433 ms vs `duckdb-decimal` 1.854 ms (duckdb-decimal 1.9× faster).
- `block-small` load vs B · portable SQL: `weftdb-stored-f64` p50 793.051 ms vs `clickhouse-decimal` 32.899 ms (clickhouse-decimal 24.1× faster).
- `block-small` interpolate vs C · client-side: `weftdb-stored-f64` p50 8.324 ms vs `clickhouse-client` 5.050 ms (clickhouse-client 1.6× faster).
- `block-small` load vs B · portable SQL: `weftdb-stored-bulk` p50 288.435 ms vs `clickhouse-decimal` 32.899 ms (clickhouse-decimal 8.8× faster).
- `block-small` interpolate vs C · client-side: `weftdb-stored-bulk` p50 22.613 ms vs `clickhouse-client` 5.050 ms (clickhouse-client 4.5× faster).
- `block-small` downsample vs B · portable SQL: `weftdb-stored-bulk` p50 3.219 ms vs `duckdb-decimal` 1.854 ms (duckdb-decimal 1.7× faster).
- `mcar-medium` interpolate vs B · portable SQL: `weftdb-memory` p50 141.324 ms vs `duckdb-decimal` 48.425 ms (duckdb-decimal 2.9× faster).
- `mcar-medium` interpolate vs C · client-side: `weftdb-memory` p50 141.324 ms vs `clickhouse-client` 15.327 ms (clickhouse-client 9.2× faster).
- `mcar-medium` downsample vs B · portable SQL: `weftdb-memory` p50 7.683 ms vs `duckdb-decimal` 4.707 ms (duckdb-decimal 1.6× faster).
- `mcar-medium` load vs B · portable SQL: `weftdb-stored` p50 5134.862 ms vs `clickhouse-double` 122.965 ms (clickhouse-double 41.8× faster).
- `mcar-medium` interpolate vs B · portable SQL: `weftdb-stored` p50 177.728 ms vs `duckdb-decimal` 48.425 ms (duckdb-decimal 3.7× faster).
- `mcar-medium` interpolate vs C · client-side: `weftdb-stored` p50 177.728 ms vs `clickhouse-client` 15.327 ms (clickhouse-client 11.6× faster).
- `mcar-medium` downsample vs B · portable SQL: `weftdb-stored` p50 17.094 ms vs `duckdb-decimal` 4.707 ms (duckdb-decimal 3.6× faster).
- `mcar-medium` load vs B · portable SQL: `weftdb-stored-f64` p50 5227.047 ms vs `clickhouse-double` 122.965 ms (clickhouse-double 42.5× faster).
- `mcar-medium` interpolate vs B · portable SQL: `weftdb-stored-f64` p50 65.351 ms vs `duckdb-decimal` 48.425 ms (duckdb-decimal 1.3× faster).
- `mcar-medium` interpolate vs C · client-side: `weftdb-stored-f64` p50 65.351 ms vs `clickhouse-client` 15.327 ms (clickhouse-client 4.3× faster).
- `mcar-medium` load vs B · portable SQL: `weftdb-stored-bulk` p50 964.077 ms vs `clickhouse-double` 122.965 ms (clickhouse-double 7.8× faster).
- `mcar-medium` interpolate vs B · portable SQL: `weftdb-stored-bulk` p50 174.457 ms vs `duckdb-decimal` 48.425 ms (duckdb-decimal 3.6× faster).
- `mcar-medium` interpolate vs C · client-side: `weftdb-stored-bulk` p50 174.457 ms vs `clickhouse-client` 15.327 ms (clickhouse-client 11.4× faster).
- `mcar-medium` downsample vs B · portable SQL: `weftdb-stored-bulk` p50 27.480 ms vs `duckdb-decimal` 4.707 ms (duckdb-decimal 5.8× faster).
- `block-medium` interpolate vs B · portable SQL: `weftdb-memory` p50 110.443 ms vs `duckdb-double` 40.857 ms (duckdb-double 2.7× faster).
- `block-medium` interpolate vs C · client-side: `weftdb-memory` p50 110.443 ms vs `clickhouse-client` 12.783 ms (clickhouse-client 8.6× faster).
- `block-medium` downsample vs B · portable SQL: `weftdb-memory` p50 5.759 ms vs `duckdb-decimal` 3.669 ms (duckdb-decimal 1.6× faster).
- `block-medium` load vs B · portable SQL: `weftdb-stored` p50 4114.217 ms vs `clickhouse-double` 103.066 ms (clickhouse-double 39.9× faster).
- `block-medium` interpolate vs B · portable SQL: `weftdb-stored` p50 138.624 ms vs `duckdb-double` 40.857 ms (duckdb-double 3.4× faster).
- `block-medium` interpolate vs C · client-side: `weftdb-stored` p50 138.624 ms vs `clickhouse-client` 12.783 ms (clickhouse-client 10.8× faster).
- `block-medium` downsample vs B · portable SQL: `weftdb-stored` p50 14.034 ms vs `duckdb-decimal` 3.669 ms (duckdb-decimal 3.8× faster).
- `block-medium` load vs B · portable SQL: `weftdb-stored-f64` p50 4116.308 ms vs `clickhouse-double` 103.066 ms (clickhouse-double 39.9× faster).
- `block-medium` interpolate vs C · client-side: `weftdb-stored-f64` p50 42.948 ms vs `clickhouse-client` 12.783 ms (clickhouse-client 3.4× faster).
- `block-medium` load vs B · portable SQL: `weftdb-stored-bulk` p50 807.529 ms vs `clickhouse-double` 103.066 ms (clickhouse-double 7.8× faster).
- `block-medium` interpolate vs B · portable SQL: `weftdb-stored-bulk` p50 129.454 ms vs `duckdb-double` 40.857 ms (duckdb-double 3.2× faster).
- `block-medium` interpolate vs C · client-side: `weftdb-stored-bulk` p50 129.454 ms vs `clickhouse-client` 12.783 ms (clickhouse-client 10.1× faster).
- `block-medium` downsample vs B · portable SQL: `weftdb-stored-bulk` p50 16.770 ms vs `duckdb-decimal` 3.669 ms (duckdb-decimal 4.6× faster).
- `uci-small` interpolate vs C · client-side: `weftdb-memory` p50 18.860 ms vs `clickhouse-client` 5.585 ms (clickhouse-client 3.4× faster).
- `uci-small` load vs B · portable SQL: `weftdb-stored` p50 887.359 ms vs `clickhouse-double` 34.024 ms (clickhouse-double 26.1× faster).
- `uci-small` interpolate vs C · client-side: `weftdb-stored` p50 23.142 ms vs `clickhouse-client` 5.585 ms (clickhouse-client 4.1× faster).
- `uci-small` downsample vs B · portable SQL: `weftdb-stored` p50 3.529 ms vs `duckdb-decimal` 2.151 ms (duckdb-decimal 1.6× faster).
- `uci-small` load vs B · portable SQL: `weftdb-stored-f64` p50 894.241 ms vs `clickhouse-double` 34.024 ms (clickhouse-double 26.3× faster).
- `uci-small` interpolate vs C · client-side: `weftdb-stored-f64` p50 9.044 ms vs `clickhouse-client` 5.585 ms (clickhouse-client 1.6× faster).
- `uci-small` load vs B · portable SQL: `weftdb-stored-bulk` p50 305.982 ms vs `clickhouse-double` 34.024 ms (clickhouse-double 9.0× faster).
- `uci-small` interpolate vs C · client-side: `weftdb-stored-bulk` p50 23.009 ms vs `clickhouse-client` 5.585 ms (clickhouse-client 4.1× faster).
- `uci-small` downsample vs B · portable SQL: `weftdb-stored-bulk` p50 3.230 ms vs `duckdb-decimal` 2.151 ms (duckdb-decimal 1.5× faster).
- `uci-medium` interpolate vs B · portable SQL: `weftdb-memory` p50 137.223 ms vs `duckdb-double` 82.015 ms (duckdb-double 1.7× faster).
- `uci-medium` interpolate vs C · client-side: `weftdb-memory` p50 137.223 ms vs `clickhouse-client` 20.093 ms (clickhouse-client 6.8× faster).
- `uci-medium` downsample vs B · portable SQL: `weftdb-memory` p50 16.609 ms vs `duckdb-decimal` 5.859 ms (duckdb-decimal 2.8× faster).
- `uci-medium` load vs B · portable SQL: `weftdb-stored` p50 5296.517 ms vs `clickhouse-double` 111.679 ms (clickhouse-double 47.4× faster).
- `uci-medium` interpolate vs B · portable SQL: `weftdb-stored` p50 168.009 ms vs `duckdb-double` 82.015 ms (duckdb-double 2.0× faster).
- `uci-medium` interpolate vs C · client-side: `weftdb-stored` p50 168.009 ms vs `clickhouse-client` 20.093 ms (clickhouse-client 8.4× faster).
- `uci-medium` downsample vs B · portable SQL: `weftdb-stored` p50 22.471 ms vs `duckdb-decimal` 5.859 ms (duckdb-decimal 3.8× faster).
- `uci-medium` load vs B · portable SQL: `weftdb-stored-f64` p50 5129.886 ms vs `clickhouse-double` 111.679 ms (clickhouse-double 45.9× faster).
- `uci-medium` interpolate vs C · client-side: `weftdb-stored-f64` p50 53.453 ms vs `clickhouse-client` 20.093 ms (clickhouse-client 2.7× faster).
- `uci-medium` load vs B · portable SQL: `weftdb-stored-bulk` p50 922.635 ms vs `clickhouse-double` 111.679 ms (clickhouse-double 8.3× faster).
- `uci-medium` interpolate vs B · portable SQL: `weftdb-stored-bulk` p50 162.153 ms vs `duckdb-double` 82.015 ms (duckdb-double 2.0× faster).
- `uci-medium` interpolate vs C · client-side: `weftdb-stored-bulk` p50 162.153 ms vs `clickhouse-client` 20.093 ms (clickhouse-client 8.1× faster).
- `uci-medium` downsample vs B · portable SQL: `weftdb-stored-bulk` p50 30.869 ms vs `duckdb-decimal` 5.859 ms (duckdb-decimal 5.3× faster).
- `tsbs-small` interpolate vs C · client-side: `weftdb-memory` p50 5.039 ms vs `questdb-client` 1.818 ms (questdb-client 2.8× faster).
- `tsbs-small` downsample vs B · portable SQL: `weftdb-memory` p50 0.929 ms vs `questdb-double` 0.585 ms (questdb-double 1.6× faster).
- `tsbs-small` load vs B · portable SQL: `weftdb-stored` p50 256.239 ms vs `clickhouse-decimal` 20.248 ms (clickhouse-decimal 12.7× faster).
- `tsbs-small` interpolate vs C · client-side: `weftdb-stored` p50 5.988 ms vs `questdb-client` 1.818 ms (questdb-client 3.3× faster).
- `tsbs-small` downsample vs B · portable SQL: `weftdb-stored` p50 1.546 ms vs `questdb-double` 0.585 ms (questdb-double 2.6× faster).
- `tsbs-small` load vs B · portable SQL: `weftdb-stored-f64` p50 267.731 ms vs `clickhouse-decimal` 20.248 ms (clickhouse-decimal 13.2× faster).
- `tsbs-small` load vs B · portable SQL: `weftdb-stored-bulk` p50 199.962 ms vs `clickhouse-decimal` 20.248 ms (clickhouse-decimal 9.9× faster).
- `tsbs-small` interpolate vs C · client-side: `weftdb-stored-bulk` p50 5.568 ms vs `questdb-client` 1.818 ms (questdb-client 3.1× faster).
- `tsbs-small` downsample vs B · portable SQL: `weftdb-stored-bulk` p50 1.496 ms vs `questdb-double` 0.585 ms (questdb-double 2.6× faster).
- `tsbs-medium` interpolate vs C · client-side: `weftdb-memory` p50 23.340 ms vs `clickhouse-client` 11.199 ms (clickhouse-client 2.1× faster).
- `tsbs-medium` downsample vs B · portable SQL: `weftdb-memory` p50 6.652 ms vs `duckdb-double` 3.051 ms (duckdb-double 2.2× faster).
- `tsbs-medium` load vs B · portable SQL: `weftdb-stored` p50 618.212 ms vs `clickhouse-double` 27.291 ms (clickhouse-double 22.7× faster).
- `tsbs-medium` interpolate vs C · client-side: `weftdb-stored` p50 27.900 ms vs `clickhouse-client` 11.199 ms (clickhouse-client 2.5× faster).
- `tsbs-medium` downsample vs B · portable SQL: `weftdb-stored` p50 9.320 ms vs `duckdb-double` 3.051 ms (duckdb-double 3.1× faster).
- `tsbs-medium` load vs B · portable SQL: `weftdb-stored-f64` p50 636.215 ms vs `clickhouse-double` 27.291 ms (clickhouse-double 23.3× faster).
- `tsbs-medium` load vs B · portable SQL: `weftdb-stored-bulk` p50 211.896 ms vs `clickhouse-double` 27.291 ms (clickhouse-double 7.8× faster).
- `tsbs-medium` interpolate vs C · client-side: `weftdb-stored-bulk` p50 27.226 ms vs `clickhouse-client` 11.199 ms (clickhouse-client 2.4× faster).
- `tsbs-medium` downsample vs B · portable SQL: `weftdb-stored-bulk` p50 10.179 ms vs `duckdb-double` 3.051 ms (duckdb-double 3.3× faster).

### Within noise

- `block-small` interpolate vs B · portable SQL: `weftdb-stored` p50 22.706 ms vs `duckdb-double` 23.080 ms (intervals overlap).
- `block-small` interpolate vs B · portable SQL: `weftdb-stored-bulk` p50 22.613 ms vs `duckdb-double` 23.080 ms (intervals overlap).
- `block-medium` interpolate vs B · portable SQL: `weftdb-stored-f64` p50 42.948 ms vs `duckdb-double` 40.857 ms (intervals overlap).
- `tsbs-small` interpolate vs C · client-side: `weftdb-stored-f64` p50 2.177 ms vs `questdb-client` 1.818 ms (intervals overlap).
- `tsbs-medium` interpolate vs C · client-side: `weftdb-stored-f64` p50 10.258 ms vs `clickhouse-client` 11.199 ms (intervals overlap).

### Not tested

- `mcar` ran at 2 of 3 scales (medium, small).
- `block` ran at 2 of 3 scales (medium, small).
- `uci` ran at 2 of 3 scales (medium, small).
- `tsbs` ran at 2 of 3 scales (medium, small).
- OS-cold and post-restart reads: every rep in this run was warm (pass `--cold-hook` for cold ones).
- TSBS's query suite (`cpu-max-all`, `high-cpu`, `groupby-orderby-limit`, `lastpoint` and the rest): those queries group many hosts' series by tag, which WeftDB stores only once tagged ingest lands (TAG-11); the `tsbs` datasets run TSBS's data through this benchmark's own phases, one host at a time.
- Gap fill, point and range reads: measured by `weft-bench` against WeftDB's own baselines, not against another engine.
- Spline (quadratic, cubic) reconstruction: none of DuckDB, ClickHouse, TimescaleDB, QuestDB, InfluxDB 3 and IoTDB has a built-in spline in the SQL these arms use (IoTDB's optional UDF library has one for its tree model, not loaded here), so only linear interpolation is compared.
- Concurrent ingest and query, many series, and long-running behaviour: covered by the soak harness, not by this report.
- Enterprise editions of QuestDB and InfluxDB 3 are excluded by their licences (see WILLINGNESS-TO-PAY.md).
- kdb+ and AVEVA PI: their licences bar benchmarking or publishing results without written consent.
- GPU backends: every arm runs splimes' automatic backend selection; GPU-specific tuning is a separate report.

### Known limitations

- DuckDB has no interpolation function. Its arm is hand-written SQL (two `ASOF JOIN`s and the first/last segment extended), which is what a DuckDB user would write, and it is gated against WeftDB's grid.
- WeftDB has no last-observation-carried-forward fill on stored data in 1.0, so no LOCF comparison is made.
- Every dataset is a single series; the grid is fixed per dataset, so input size grows with scale while output size does not.
- Load layouts differ by arm: `weftdb-stored` and `weftdb-stored-f64` seal 1,024-row durable segments (the layout the stored read path is measured on), `weftdb-stored-bulk` seals one segment per load, and DuckDB does one bulk append and one CHECKPOINT. `weftdb-stored-bulk` is the like-for-like load comparison.
- `duckdb-decimal` stores the knots exactly as `DECIMAL(18, s)`, but its interpolation line is computed in `DOUBLE` (DuckDB's `DECIMAL × DOUBLE` is `DOUBLE`), as is WeftDB's (splimes computes in `f64`). Neither engine interpolates in exact decimal arithmetic.
- ClickHouse's own gap filling (`WITH FILL … INTERPOLATE`) carries the previous row's expression forward rather than drawing a line between neighbours, so its arm runs the same hand-written `ASOF JOIN` SQL as DuckDB's, gated the same way.
- ClickHouse is a server; the other engines run in-process. Its timings include a loopback HTTP round trip and `RowBinary` encoding both ways, which is what a ClickHouse client pays, and its load is one `INSERT` into a `MergeTree` part with `fsync_after_insert` on.
- `clickhouse-decimal` stores the knots exactly as `Decimal64(s)`; its interpolation line is computed in `Float64`, since ClickHouse refuses `Decimal × Float64` without a cast.
- TimescaleDB's `time_bucket_gapfill` with `interpolate()` draws its line between bucket aggregates at bucket boundaries, not between the knots, and does not extend past the data, so on jittered knots it computes a different answer. Its arm is the SQL a PostgreSQL user writes without `ASOF JOIN`: two index-backed `LATERAL … LIMIT 1` lookups per grid instant, gated like every arm.
- TimescaleDB is a PostgreSQL server; its timings include the loopback wire protocol, and its load is one text `COPY` committed with `synchronous_commit` on, into a hypertable whose one chunk spans the dataset. The hypertable stays in the row store: TimescaleDB's columnstore (compression) is not enabled, so its bytes on disk are uncompressed.
- QuestDB's `SAMPLE BY … FILL(LINEAR)` fills between bucket aggregates, not between the knots, so its arm builds segments with the native `LT JOIN` and finds each grid instant's segment with an `ASOF JOIN`, gated like every arm.
- Open-source QuestDB acknowledges that a write was received, not that it is durable (a durable acknowledgement needs Enterprise). Its load is timed from the first QWP frame until the WAL is applied and the rows are visible, and the engine line records the server's `cairo.commit.mode`; under `sync` each commit is synced to disk. In `questdb-decimal` the bucket mean is `avg(v::double)`: QuestDB's own `avg` of a `DECIMAL` column rounds the mean to the column's scale. Its bytes on disk are `table_partitions()`'s, which include the space QuestDB reserves ahead in the active partition, so small tables report far more than they hold.
- InfluxDB 3 has no decimal type, so it has one arm, `influxdb3-double`, and no exact-decimal comparison. Its SQL engine (DataFusion) has no `ASOF JOIN` and `date_bin_gapfill`'s `interpolate()` fills between bucket aggregates, so the arm merges the grid with the knots and finds each grid row's neighbouring knots with running `max`/`min` windows over the knot instants, gated like every arm.
- InfluxDB 3's write answers once the WAL file holding the rows is persisted (`no_sync=false`, its default), and the server flushes its WAL every second (`--wal-flush-interval`, default `1s`), so a load takes about a second however small it is; a load is sent as concurrent writes of at most 8 MiB, under the server's 10 MiB request cap, so they share the WAL flush. A series and an instant are its key, so a second write at the same instant replaces the first: buckets count a duplicated instant once where the reference counts every row (this benchmark's datasets have none). Its bytes on disk are not reported: the rows sit in the WAL until the server snapshots them to Parquet on its own schedule. InfluxDB 3 Core refuses a query that would read more than 432 Parquet files, and it writes a file per ten minutes of data time with no compaction, so the four-year UCI series is about 200,000 files once snapshotted; the reference server raises that cap with `--query-file-limit 300000`, and the UCI interpolation times show the cost of reading that many files.
- IoTDB has no decimal type, so it has one arm, `iotdb-double`, and no exact-decimal comparison. Its SQL has no series generator, so the grid comes from `date_bin_gapfill` over the grid's span; segments are paired by `lead` and found by a native `ASOF LEFT JOIN`. A device and an instant are IoTDB's key, so a second write at the same instant replaces the first: buckets count a duplicated instant once where the reference counts every row (this benchmark's datasets have none).
- IoTDB is reached through its table-model REST API, which answers in JSON; its own clients speak Thrift, which has no Rust client, so its reads carry a text encoding the binary paths of the other servers do not. Writes are `insertTablet` requests of 100,000 rows in time order, each answered once its WAL entry is synced (`wal_mode=SYNC`, which the reference server sets; IoTDB's default is `ASYNC`). Its bytes on disk are the table's TsFiles after a `FLUSH` issued once the timed loads are done.
- The `tsbs` datasets are one host's (`host_0`) `usage_user` from TSBS's `cpu-only` generator: an integer random walk read every ten seconds with no gaps, over the synthetic datasets' span, reconstructed per second. So each grid instant falls between knots ten seconds apart, and nine in ten instants are interpolated; there is no ground truth, so only the reference gate applies.

### Configuration caveats

- Hardware: AMD Ryzen 9 7950X3D 16-Core Processor (16 physical / 16 logical cores, 125 GiB memory). The working directory was on a ssd ext4 disk at /mnt/weftbench.
- The in-process engines wrote their stores to `/mnt/weftbench/tmp` (`TMPDIR`), which may be a different disk from the working directory.
- The IoTDB server stored its data in its own data directory, which this report cannot see; check the reproduction notes for where it was mounted.
- The InfluxDB 3 server stored its data in its own object-store directory, which this report cannot see; check the reproduction notes for where it was mounted.
- The QuestDB server stored its data in its own data directory, which this report cannot see; check the reproduction notes for where it was mounted.
- The TimescaleDB server stored its data in its own data directory, which this report cannot see; check the reproduction notes for where it was mounted.
- The ClickHouse server stored its data in its own data directory, which this report cannot see; check the reproduction notes for where it was mounted.
- Pooled from 2 separate runs; every interval is drawn runs-first and a win or loss needs every run's median to agree. The widest p50 spread between runs was 200.0%.
- **Runs answered differently** for mcar-medium / timescaledb-double, which fails those gates.
- Some results have only 6 reps, so their intervals are wide and fewer comparisons can separate from noise.

### Reproduction

### Run manifest

- uci `/mnt/weftbench/data/uci-household-power/household_power_consumption.txt`: 132960755 bytes, sha256 `4259c9d7ece5dbee9ab8d53682baac68d791c864f0f64a52b4043cb3b90894b7`.
- tsbs `/mnt/weftbench/tsbs/cpu-only.lp`: 207025857 bytes, sha256 `30cd624efb6a2d11cec55d4822ddbab8df20fd570f26124ddd92c51fedcee659`.
- run `/mnt/weftbench/xbench-508-run1/xbench.json`: 976144 bytes, sha256 `a260b080e12b56830395a44232d148b24eddf45e8a1b7d18c511af840a1f76b1`.
- run `/mnt/weftbench/xbench-508-run2/xbench.json`: 975919 bytes, sha256 `9c1042801750b697989183678bcc11457baef8e634cc1757f0a5cd8713d210eb`.
- Binary sha256 `fa55d4e1eea9d356ff632befad8239c7b4b514dbea5a8465916290f69df7e6f9`.
- clickhouse container `weft-xbench-clickhouse`: image `clickhouse/clickhouse-server:latest` (`sha256:739e099abd1157390960a59e4532d21781d8fde57e6e4151cb920ea02c30b1ce`), command `/entrypoint.sh`, environment `CLICKHOUSE_SKIP_USER_SETUP=1 PATH=/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin LANG=en_US.UTF-8 TZ=UTC CLICKHOUSE_CONFIG=/etc/clickhouse-server/config.xml`.
- timescaledb container `weft-xbench-timescale`: image `timescale/timescaledb:latest-pg17` (`sha256:b56a3f51991df1f79d0c1a0841cad5c5fa8058fdd6a3ee6886394a6d4c174538`), command `docker-entrypoint.sh postgres -c timescaledb.telemetry_level=off`, environment `POSTGRES_PASSWORD=<redacted> POSTGRES_DB=xbench PATH=/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin GOSU_VERSION=1.19 LANG=en_US.utf8 PG_MAJOR=17 PG_VERSION=17.11 PG_SHA256=dd27f2b3c59e73ed14aa3324901242bf69a032a6347805f274e6260322d42979 DOCKER_PG_LLVM_DEPS=llvm21-dev 		clang21 PGDATA=/var/lib/postgresql/data`.
- questdb container `weft-xbench-questdb`: image `questdb/questdb:latest` (`sha256:3b11dc5f77aa299c7c9191b10cf50124c9e97663ce93bc50df5040374e87efc9`), command `/docker-entrypoint.sh`, environment `QDB_TELEMETRY_ENABLED=false QDB_CAIRO_COMMIT_MODE=sync PATH=/usr/local/bin:/usr/bin container=oci`.
- influxdb3 container `weft-xbench-influxdb3`: image `influxdb:3-core` (`sha256:26875ba5ae6e843fdbd208b1ac4a55fa1d2ec3aa7faf2d0d1b5035cb1f998990`), command `/usr/bin/entrypoint.sh influxdb3 serve --node-id xbench --object-store file --data-dir /var/lib/influxdb3 --without-auth --disable-telemetry-upload --query-file-limit 300000`, environment `PATH=/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin INFLUXDB_VERSION=3.12.2 INFLUXDB3_PLUGIN_DIR=/plugins INFLUXDB3_DATA_DIR=/home/influxdb3/.influxdb3 INFLUXDB3_SERVE_INVOCATION_METHOD=docker-hub INFLUXDB_IOX_DB_DIR=/var/lib/influxdb3 INFLUXDB3_LOG_FILTER=info`.
- iotdb container `weft-xbench-iotdb`: image `apache/iotdb:2.0.11-standalone` (`sha256:5ace0f3eceff892dd349d02288224c966f3fdeb11d08c4f575b642a110b82338`), command `/usr/bin/dumb-init -- bash -c entrypoint.sh all`, environment `timestamp_precision=ns wal_mode=SYNC rest_query_default_row_size_limit=10000000 enable_rest_service=true PATH=/iotdb/sbin/:/iotdb/tools/:/opt/java/openjdk/bin:/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin JAVA_HOME=/opt/java/openjdk LANG=en_US.UTF-8 LANGUAGE=en_US:en LC_ALL=en_US.UTF-8 JAVA_VERSION=jdk-17.0.15+6 IOTDB_HOME=/iotdb VERSION=2.0.11`.

- Check out commit `4531d48347167ad4d9749fec45dcc3a060ec7316`.
- Build: `cargo build --release --manifest-path weft-xbench/Cargo.toml` (DuckDB compiles from C++; put `CARGO_TARGET_DIR` on a fast local disk).
- Run: `TMPDIR=<scratch on the disk under test> weft-xbench --clickhouse http://127.0.0.1:18123 --timescale postgresql://postgres:<redacted>@127.0.0.1:15432/xbench --questdb 127.0.0.1:19000 --influxdb3 http://127.0.0.1:18181 --iotdb http://127.0.0.1:18080 --out /mnt/weftbench/xbench-508-run1 --scales small,medium --reps 10 --load-reps 3 --uci /mnt/weftbench/data/uci-household-power/household_power_consumption.txt --tsbs /mnt/weftbench/tsbs/cpu-only.lp --container clickhouse=weft-xbench-clickhouse --container timescaledb=weft-xbench-timescale --container questdb=weft-xbench-questdb --container influxdb3=weft-xbench-influxdb3 --container iotdb=weft-xbench-iotdb`.
- For the ClickHouse arms, a server on the same disk: `docker run -d -p 127.0.0.1:8123:8123 -v <disk under test>/clickhouse:/var/lib/clickhouse clickhouse/clickhouse-server:<version above>`, then `--clickhouse http://127.0.0.1:8123`.
- For the TimescaleDB arms, likewise: `docker run -d -p 127.0.0.1:5432:5432 -e POSTGRES_PASSWORD=<password> -e POSTGRES_DB=xbench -v <disk under test>/timescale:/var/lib/postgresql/data timescale/timescaledb:<version above> postgres -c timescaledb.telemetry_level=off`, then `--timescale postgresql://postgres:<password>@127.0.0.1:5432/xbench`.
- For the QuestDB arms: `docker run -d -p 127.0.0.1:9000:9000 -e QDB_TELEMETRY_ENABLED=false -e QDB_CAIRO_COMMIT_MODE=sync -v <disk under test>/questdb:/var/lib/questdb questdb/questdb:<version above>`, then `--questdb 127.0.0.1:9000`.
- For the InfluxDB 3 arm: `docker run -d -p 127.0.0.1:8181:8181 -v <disk under test>/influxdb3:/var/lib/influxdb3 influxdb:<version above> influxdb3 serve --node-id xbench --object-store file --data-dir /var/lib/influxdb3 --without-auth --disable-telemetry-upload --query-file-limit 300000`, then `--influxdb3 http://127.0.0.1:8181`.
- For the IoTDB arm: `docker run -d -p 127.0.0.1:18080:18080 -e enable_rest_service=true -e timestamp_precision=ns -e wal_mode=SYNC -e rest_query_default_row_size_limit=10000000 -v <disk under test>/iotdb:/iotdb/data apache/iotdb:<version above>-standalone` (the precision must be set before the first start), then `--iotdb http://127.0.0.1:18080`.
- For the `tsbs` datasets: build TSBS's generator at the pinned commit, `GOBIN=<dir> go install github.com/timescale/tsbs/cmd/tsbs_generate_data@8323e59c74027b108f4ad5ec5d3e498b0101a02e`, write a week of `cpu-only` data, `tsbs_generate_data --use-case=cpu-only --seed=123 --scale=10 --log-interval=10s --timestamp-start=2016-01-01T00:00:00Z --timestamp-end=2016-01-08T00:00:10Z --format=influx > cpu-only.lp`, then `--tsbs cpu-only.lp`.
- Render: `weft-xbench render <xbench.json> --out <dir>`. The synthetic datasets are regenerated from seed 1592649491.

