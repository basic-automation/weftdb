# WeftDB cross-engine benchmark

Generated 2026-10-30T03:17:22.424188325+00:00 on AMD Ryzen 9 7950X3D 16-Core Processor (linux x86_64). Engines: weftdb 0.1.0, duckdb v2.0.0. Source commit: 9903465fe0a3f286787957da7fa135d23ac04a72. Seed: 1592649491.

## Results

Latency in milliseconds: p50 with its bootstrap interval, and p95. Accuracy is RMSE against the analytic ground truth, for synthetic datasets. B/pt is the store's size on disk after a load, divided by the points loaded (whole files: indexes and headers included).

| dataset | phase | arm | input | p50 [CI] | p95 | reps | gate | RMSE | B/pt |
| --- | --- | --- | --- | --- | --- | --- | --- | --- | --- |
| mcar-tiny | interpolate | weftdb-memory | 3560 | 6.734 [4.722, 11.723] | 17.603 | 10 | PASS | 0.9453 | — |
| mcar-tiny | downsample | weftdb-memory | 3560 | 0.972 [0.969, 0.982] | 0.999 | 10 | PASS | — | — |
| mcar-tiny | load | weftdb-stored | 3560 | 211.690 [205.989, 222.578] | 222.578 | 5 | — | — | 20.70 |
| mcar-tiny | interpolate | weftdb-stored | 3560 | 4.620 [3.905, 5.168] | 6.311 | 10 | PASS | 0.9453 | — |
| mcar-tiny | downsample | weftdb-stored | 3560 | 1.231 [1.188, 1.337] | 1.740 | 10 | PASS | — | — |
| mcar-tiny | load | weftdb-stored-f64 | 3560 | 200.596 [200.011, 204.061] | 204.061 | 5 | — | — | 20.70 |
| mcar-tiny | interpolate | weftdb-stored-f64 | 3560 | 1.236 [1.142, 1.314] | 1.691 | 10 | PASS | 0.9453 | — |
| mcar-tiny | load | weftdb-stored-bulk | 3560 | 183.503 [181.764, 184.641] | 184.641 | 5 | — | — | 20.27 |
| mcar-tiny | interpolate | weftdb-stored-bulk | 3560 | 3.695 [3.499, 3.888] | 4.554 | 10 | PASS | 0.9453 | — |
| mcar-tiny | downsample | weftdb-stored-bulk | 3560 | 1.286 [1.260, 1.353] | 1.519 | 10 | PASS | — | — |
| mcar-tiny | load | duckdb-double | 3560 | 19.545 [18.964, 24.960] | 24.960 | 5 | — | — | 150.72 |
| mcar-tiny | interpolate | duckdb-double | 3560 | 18.837 [16.950, 19.630] | 22.874 | 10 | PASS | 0.9453 | — |
| mcar-tiny | downsample | duckdb-double | 3560 | 1.014 [0.959, 1.418] | 2.464 | 10 | PASS | — | — |
| mcar-tiny | load | duckdb-decimal | 3560 | 20.506 [20.063, 23.075] | 23.075 | 5 | — | — | 150.72 |
| mcar-tiny | interpolate | duckdb-decimal | 3560 | 17.330 [16.702, 18.773] | 23.902 | 10 | PASS | 0.9453 | — |
| mcar-tiny | downsample | duckdb-decimal | 3560 | 1.107 [1.035, 1.783] | 3.006 | 10 | PASS | — | — |
| block-tiny | interpolate | weftdb-memory | 2864 | 4.156 [4.087, 4.385] | 4.780 | 10 | PASS | 0.9965 | — |
| block-tiny | downsample | weftdb-memory | 2864 | 0.857 [0.825, 0.875] | 0.876 | 10 | PASS | — | — |
| block-tiny | load | weftdb-stored | 2864 | 200.161 [197.605, 201.256] | 201.256 | 5 | — | — | 23.87 |
| block-tiny | interpolate | weftdb-stored | 2864 | 4.726 [4.585, 5.440] | 5.671 | 10 | PASS | 0.9965 | — |
| block-tiny | downsample | weftdb-stored | 2864 | 1.206 [1.148, 1.404] | 1.663 | 10 | PASS | — | — |
| block-tiny | load | weftdb-stored-f64 | 2864 | 203.540 [194.064, 208.797] | 208.797 | 5 | — | — | 23.88 |
| block-tiny | interpolate | weftdb-stored-f64 | 2864 | 1.516 [1.448, 1.933] | 2.334 | 10 | PASS | 0.9965 | — |
| block-tiny | load | weftdb-stored-bulk | 2864 | 191.349 [187.388, 193.028] | 193.028 | 5 | — | — | 23.52 |
| block-tiny | interpolate | weftdb-stored-bulk | 2864 | 4.667 [4.374, 4.796] | 6.006 | 10 | PASS | 0.9965 | — |
| block-tiny | downsample | weftdb-stored-bulk | 2864 | 0.996 [0.990, 1.017] | 1.922 | 10 | PASS | — | — |
| block-tiny | load | duckdb-double | 2864 | 21.205 [19.971, 26.202] | 26.202 | 5 | — | — | 187.35 |
| block-tiny | interpolate | duckdb-double | 2864 | 18.890 [17.914, 19.266] | 24.048 | 10 | PASS | 0.9965 | — |
| block-tiny | downsample | duckdb-double | 2864 | 1.138 [1.038, 1.368] | 2.195 | 10 | PASS | — | — |
| block-tiny | load | duckdb-decimal | 2864 | 22.021 [21.088, 24.593] | 24.593 | 5 | — | — | 187.35 |
| block-tiny | interpolate | duckdb-decimal | 2864 | 18.558 [17.503, 19.094] | 23.578 | 10 | PASS | 0.9965 | — |
| block-tiny | downsample | duckdb-decimal | 2864 | 1.203 [1.030, 1.557] | 2.055 | 10 | PASS | — | — |
| mcar-small | interpolate | weftdb-memory | 86245 | 24.087 [22.707, 25.842] | 26.416 | 10 | PASS | 0.9430 | — |
| mcar-small | downsample | weftdb-memory | 86245 | 1.745 [1.715, 1.806] | 2.026 | 10 | PASS | — | — |
| mcar-small | load | weftdb-stored | 86245 | 927.833 [898.741, 970.632] | 970.632 | 5 | — | — | 7.97 |
| mcar-small | interpolate | weftdb-stored | 86245 | 29.203 [28.728, 30.917] | 31.914 | 10 | PASS | 0.9430 | — |
| mcar-small | downsample | weftdb-stored | 86245 | 4.334 [3.954, 4.795] | 5.081 | 10 | PASS | — | — |
| mcar-small | load | weftdb-stored-f64 | 86245 | 901.611 [892.678, 937.217] | 937.217 | 5 | — | — | 7.98 |
| mcar-small | interpolate | weftdb-stored-f64 | 86245 | 8.396 [8.140, 10.702] | 11.221 | 10 | PASS | 0.9430 | — |
| mcar-small | load | weftdb-stored-bulk | 86245 | 297.364 [292.523, 300.177] | 300.177 | 5 | — | — | 7.48 |
| mcar-small | interpolate | weftdb-stored-bulk | 86245 | 27.648 [26.668, 28.391] | 30.394 | 10 | PASS | 0.9430 | — |
| mcar-small | downsample | weftdb-stored-bulk | 86245 | 4.827 [4.596, 5.210] | 8.094 | 10 | PASS | — | — |
| mcar-small | load | duckdb-double | 86245 | 57.071 [50.738, 60.939] | 60.939 | 5 | — | — | 12.30 |
| mcar-small | interpolate | duckdb-double | 86245 | 24.978 [24.524, 29.384] | 34.029 | 10 | PASS | 0.9430 | — |
| mcar-small | downsample | duckdb-double | 86245 | 2.221 [1.944, 2.277] | 3.272 | 10 | PASS | — | — |
| mcar-small | load | duckdb-decimal | 86245 | 57.675 [54.793, 61.221] | 61.221 | 5 | — | — | 12.30 |
| mcar-small | interpolate | duckdb-decimal | 86245 | 23.617 [23.349, 27.139] | 32.896 | 10 | PASS | 0.9430 | — |
| mcar-small | downsample | duckdb-decimal | 86245 | 1.975 [1.835, 2.108] | 3.014 | 10 | PASS | — | — |
| block-small | interpolate | weftdb-memory | 68751 | 19.509 [18.973, 20.088] | 21.377 | 10 | PASS | 0.9504 | — |
| block-small | downsample | weftdb-memory | 68751 | 1.406 [1.362, 1.452] | 1.582 | 10 | PASS | — | — |
| block-small | load | weftdb-stored | 68751 | 766.492 [761.953, 785.240] | 785.240 | 5 | — | — | 8.04 |
| block-small | interpolate | weftdb-stored | 68751 | 24.026 [23.821, 25.774] | 29.206 | 10 | PASS | 0.9504 | — |
| block-small | downsample | weftdb-stored | 68751 | 3.217 [2.989, 3.291] | 3.834 | 10 | PASS | — | — |
| block-small | load | weftdb-stored-f64 | 68751 | 766.026 [744.657, 818.548] | 818.548 | 5 | — | — | 8.05 |
| block-small | interpolate | weftdb-stored-f64 | 68751 | 6.134 [5.936, 6.836] | 7.303 | 10 | PASS | 0.9504 | — |
| block-small | load | weftdb-stored-bulk | 68751 | 267.073 [263.952, 281.353] | 281.353 | 5 | — | — | 7.56 |
| block-small | interpolate | weftdb-stored-bulk | 68751 | 22.696 [22.142, 24.016] | 25.538 | 10 | PASS | 0.9504 | — |
| block-small | downsample | weftdb-stored-bulk | 68751 | 3.577 [3.305, 4.041] | 5.800 | 10 | PASS | — | — |
| block-small | load | duckdb-double | 68751 | 49.848 [45.079, 59.449] | 59.449 | 5 | — | — | 15.43 |
| block-small | interpolate | duckdb-double | 68751 | 21.747 [21.001, 23.286] | 28.457 | 10 | PASS | 0.9504 | — |
| block-small | downsample | duckdb-double | 68751 | 1.799 [1.755, 1.839] | 3.173 | 10 | PASS | — | — |
| block-small | load | duckdb-decimal | 68751 | 49.987 [48.533, 54.878] | 54.878 | 5 | — | — | 15.43 |
| block-small | interpolate | duckdb-decimal | 68751 | 22.574 [20.808, 24.093] | 26.511 | 10 | PASS | 0.9504 | — |
| block-small | downsample | duckdb-decimal | 68751 | 1.880 [1.796, 2.193] | 2.988 | 10 | PASS | — | — |
| mcar-medium | interpolate | weftdb-memory | 604689 | 142.803 [140.749, 147.496] | 153.335 | 10 | PASS | 0.9407 | — |
| mcar-medium | downsample | weftdb-memory | 604689 | 7.919 [7.722, 8.300] | 8.802 | 10 | PASS | — | — |
| mcar-medium | load | weftdb-stored | 604689 | 5555.821 [5176.703, 5710.144] | 5710.144 | 5 | — | — | 7.17 |
| mcar-medium | interpolate | weftdb-stored | 604689 | 198.326 [190.333, 205.275] | 214.569 | 10 | PASS | 0.9407 | — |
| mcar-medium | downsample | weftdb-stored | 604689 | 17.879 [17.166, 20.408] | 21.378 | 10 | PASS | — | — |
| mcar-medium | load | weftdb-stored-f64 | 604689 | 5535.440 [5307.294, 5612.711] | 5612.711 | 5 | — | — | 7.17 |
| mcar-medium | interpolate | weftdb-stored-f64 | 604689 | 67.711 [59.144, 71.727] | 95.536 | 10 | PASS | 0.9407 | — |
| mcar-medium | load | weftdb-stored-bulk | 604689 | 964.127 [960.843, 973.511] | 973.511 | 5 | — | — | 6.66 |
| mcar-medium | interpolate | weftdb-stored-bulk | 604689 | 210.524 [187.510, 216.460] | 224.594 | 10 | PASS | 0.9407 | — |
| mcar-medium | downsample | weftdb-stored-bulk | 604689 | 44.466 [43.010, 45.543] | 47.430 | 10 | PASS | — | — |
| mcar-medium | load | duckdb-double | 604689 | 204.262 [199.480, 225.541] | 225.541 | 5 | — | — | 8.26 |
| mcar-medium | interpolate | duckdb-double | 604689 | 50.859 [49.692, 52.716] | 58.176 | 10 | PASS | 0.9407 | — |
| mcar-medium | downsample | duckdb-double | 604689 | 5.371 [4.958, 5.834] | 6.522 | 10 | PASS | — | — |
| mcar-medium | load | duckdb-decimal | 604689 | 277.870 [250.354, 281.253] | 281.253 | 5 | — | — | 8.26 |
| mcar-medium | interpolate | duckdb-decimal | 604689 | 53.428 [48.399, 58.519] | 60.382 | 10 | PASS | 0.9407 | — |
| mcar-medium | downsample | duckdb-decimal | 604689 | 6.517 [6.116, 7.226] | 8.274 | 10 | PASS | — | — |
| block-medium | interpolate | weftdb-memory | 481252 | 112.244 [109.839, 115.776] | 119.110 | 10 | PASS | 0.9357 | — |
| block-medium | downsample | weftdb-memory | 481252 | 5.774 [5.493, 5.885] | 6.309 | 10 | PASS | — | — |
| block-medium | load | weftdb-stored | 481252 | 4246.923 [4175.948, 4274.915] | 4274.915 | 5 | — | — | 7.13 |
| block-medium | interpolate | weftdb-stored | 481252 | 152.038 [148.639, 157.709] | 167.179 | 10 | PASS | 0.9357 | — |
| block-medium | downsample | weftdb-stored | 481252 | 13.111 [12.244, 13.698] | 14.096 | 10 | PASS | — | — |
| block-medium | load | weftdb-stored-f64 | 481252 | 4272.610 [4191.821, 4336.625] | 4336.625 | 5 | — | — | 7.13 |
| block-medium | interpolate | weftdb-stored-f64 | 481252 | 45.520 [41.619, 50.892] | 53.425 | 10 | PASS | 0.9357 | — |
| block-medium | load | weftdb-stored-bulk | 481252 | 802.981 [798.111, 803.356] | 803.356 | 5 | — | — | 6.62 |
| block-medium | interpolate | weftdb-stored-bulk | 481252 | 149.209 [143.379, 150.548] | 168.751 | 10 | PASS | 0.9357 | — |
| block-medium | downsample | weftdb-stored-bulk | 481252 | 35.164 [35.009, 35.905] | 36.365 | 10 | PASS | — | — |
| block-medium | load | duckdb-double | 481252 | 180.401 [168.778, 184.803] | 184.803 | 5 | — | — | 8.74 |
| block-medium | interpolate | duckdb-double | 481252 | 42.999 [41.513, 46.615] | 52.851 | 10 | PASS | 0.9357 | — |
| block-medium | downsample | duckdb-double | 481252 | 4.148 [3.843, 4.557] | 4.879 | 10 | PASS | — | — |
| block-medium | load | duckdb-decimal | 481252 | 203.652 [200.852, 231.695] | 231.695 | 5 | — | — | 8.20 |
| block-medium | interpolate | duckdb-decimal | 481252 | 46.368 [43.907, 48.103] | 52.684 | 10 | PASS | 0.9357 | — |
| block-medium | downsample | duckdb-decimal | 481252 | 4.392 [3.769, 5.222] | 6.714 | 10 | PASS | — | — |
| uci-tiny | interpolate | weftdb-memory | 3600 | 1.038 [0.981, 1.045] | 1.129 | 10 | PASS | — | — |
| uci-tiny | downsample | weftdb-memory | 3600 | 0.082 [0.076, 0.096] | 0.180 | 10 | PASS | — | — |
| uci-tiny | load | weftdb-stored | 3600 | 214.496 [211.830, 232.521] | 232.521 | 5 | — | — | 14.23 |
| uci-tiny | interpolate | weftdb-stored | 3600 | 1.386 [1.272, 1.438] | 1.718 | 10 | PASS | — | — |
| uci-tiny | downsample | weftdb-stored | 3600 | 0.306 [0.258, 0.335] | 0.417 | 10 | PASS | — | — |
| uci-tiny | load | weftdb-stored-f64 | 3600 | 225.491 [209.395, 227.809] | 227.809 | 5 | — | — | 14.23 |
| uci-tiny | interpolate | weftdb-stored-f64 | 3600 | 0.553 [0.519, 0.674] | 1.276 | 10 | PASS | — | — |
| uci-tiny | load | weftdb-stored-bulk | 3600 | 190.931 [189.376, 193.680] | 193.680 | 5 | — | — | 13.82 |
| uci-tiny | interpolate | weftdb-stored-bulk | 3600 | 1.146 [1.125, 1.256] | 2.112 | 10 | PASS | — | — |
| uci-tiny | downsample | weftdb-stored-bulk | 3600 | 0.269 [0.264, 0.323] | 0.379 | 10 | PASS | — | — |
| uci-tiny | load | duckdb-double | 3600 | 24.724 [21.387, 25.196] | 25.196 | 5 | — | — | 149.05 |
| uci-tiny | interpolate | duckdb-double | 3600 | 8.425 [8.239, 9.109] | 11.689 | 10 | PASS | — | — |
| uci-tiny | downsample | duckdb-double | 3600 | 0.900 [0.839, 1.065] | 3.022 | 10 | PASS | — | — |
| uci-tiny | load | duckdb-decimal | 3600 | 23.562 [21.091, 25.182] | 25.182 | 5 | — | — | 149.05 |
| uci-tiny | interpolate | duckdb-decimal | 3600 | 9.208 [8.944, 9.625] | 11.733 | 10 | PASS | — | — |
| uci-tiny | downsample | duckdb-decimal | 3600 | 1.021 [0.956, 1.129] | 1.836 | 10 | PASS | — | — |
| uci-small | interpolate | weftdb-memory | 86400 | 20.481 [20.004, 21.078] | 24.073 | 10 | PASS | — | — |
| uci-small | downsample | weftdb-memory | 86400 | 1.835 [1.679, 2.478] | 3.450 | 10 | PASS | — | — |
| uci-small | load | weftdb-stored | 86400 | 907.852 [903.712, 919.786] | 919.786 | 5 | — | — | 2.38 |
| uci-small | interpolate | weftdb-stored | 86400 | 28.044 [26.222, 28.413] | 28.697 | 10 | PASS | — | — |
| uci-small | downsample | weftdb-stored | 86400 | 3.040 [2.800, 3.955] | 4.640 | 10 | PASS | — | — |
| uci-small | load | weftdb-stored-f64 | 86400 | 915.667 [904.163, 922.542] | 922.542 | 5 | — | — | 2.38 |
| uci-small | interpolate | weftdb-stored-f64 | 86400 | 9.612 [8.474, 10.554] | 12.075 | 10 | PASS | — | — |
| uci-small | load | weftdb-stored-bulk | 86400 | 297.612 [296.375, 300.291] | 300.291 | 5 | — | — | 1.90 |
| uci-small | interpolate | weftdb-stored-bulk | 86400 | 23.474 [23.173, 24.088] | 24.641 | 10 | PASS | — | — |
| uci-small | downsample | weftdb-stored-bulk | 86400 | 3.753 [3.336, 4.299] | 4.690 | 10 | PASS | — | — |
| uci-small | load | duckdb-double | 86400 | 53.643 [49.707, 55.680] | 55.680 | 5 | — | — | 6.21 |
| uci-small | interpolate | duckdb-double | 86400 | 25.714 [24.298, 27.412] | 31.031 | 10 | PASS | — | — |
| uci-small | downsample | duckdb-double | 86400 | 2.184 [2.021, 2.331] | 3.118 | 10 | PASS | — | — |
| uci-small | load | duckdb-decimal | 86400 | 57.510 [55.835, 60.774] | 60.774 | 5 | — | — | 6.21 |
| uci-small | interpolate | duckdb-decimal | 86400 | 25.776 [24.130, 27.188] | 36.209 | 10 | PASS | — | — |
| uci-small | downsample | duckdb-decimal | 86400 | 2.237 [2.102, 2.581] | 3.199 | 10 | PASS | — | — |
| uci-medium | interpolate | weftdb-memory | 600863 | 140.672 [139.271, 145.495] | 158.541 | 10 | PASS | — | — |
| uci-medium | downsample | weftdb-memory | 600863 | 15.807 [15.711, 16.138] | 16.475 | 10 | PASS | — | — |
| uci-medium | load | weftdb-stored | 600863 | 5294.161 [5194.205, 5333.021] | 5333.021 | 5 | — | — | 1.89 |
| uci-medium | interpolate | weftdb-stored | 600863 | 191.404 [175.784, 204.923] | 226.802 | 10 | PASS | — | — |
| uci-medium | downsample | weftdb-stored | 600863 | 21.966 [21.204, 22.582] | 24.460 | 10 | PASS | — | — |
| uci-medium | load | weftdb-stored-f64 | 600863 | 5303.131 [5233.251, 5366.054] | 5366.054 | 5 | — | — | 1.89 |
| uci-medium | interpolate | weftdb-stored-f64 | 600863 | 58.268 [54.710, 62.768] | 68.553 | 10 | PASS | — | — |
| uci-medium | load | weftdb-stored-bulk | 600863 | 923.009 [919.847, 932.248] | 932.248 | 5 | — | — | 1.39 |
| uci-medium | interpolate | weftdb-stored-bulk | 600863 | 169.015 [168.115, 172.180] | 209.250 | 10 | PASS | — | — |
| uci-medium | downsample | weftdb-stored-bulk | 600863 | 35.885 [35.024, 36.409] | 37.194 | 10 | PASS | — | — |
| uci-medium | load | duckdb-double | 600863 | 196.959 [194.965, 200.416] | 200.416 | 5 | — | — | 3.07 |
| uci-medium | interpolate | duckdb-double | 600863 | 81.883 [76.907, 86.922] | 87.680 | 10 | PASS | — | — |
| uci-medium | downsample | duckdb-double | 600863 | 6.370 [5.877, 6.837] | 9.160 | 10 | PASS | — | — |
| uci-medium | load | duckdb-decimal | 600863 | 237.258 [235.584, 238.054] | 238.054 | 5 | — | — | 3.07 |
| uci-medium | interpolate | duckdb-decimal | 600863 | 83.414 [80.226, 88.046] | 92.822 | 10 | PASS | — | — |
| uci-medium | downsample | duckdb-decimal | 600863 | 6.275 [6.156, 6.940] | 7.111 | 10 | PASS | — | — |

## Honesty

Generated from the results above. A WeftDB arm wins or loses only when its p50 interval does not overlap the fastest other engine's; overlapping intervals are within noise.

### Where WeftDB wins

- `mcar-tiny` interpolate: `weftdb-memory` p50 6.734 ms vs `duckdb-decimal` 17.330 ms (weftdb-memory 2.6× faster).
- `mcar-tiny` interpolate: `weftdb-stored` p50 4.620 ms vs `duckdb-decimal` 17.330 ms (weftdb-stored 3.8× faster).
- `mcar-tiny` interpolate: `weftdb-stored-f64` p50 1.236 ms vs `duckdb-decimal` 17.330 ms (weftdb-stored-f64 14.0× faster).
- `mcar-tiny` interpolate: `weftdb-stored-bulk` p50 3.695 ms vs `duckdb-decimal` 17.330 ms (weftdb-stored-bulk 4.7× faster).
- `block-tiny` interpolate: `weftdb-memory` p50 4.156 ms vs `duckdb-decimal` 18.558 ms (weftdb-memory 4.5× faster).
- `block-tiny` downsample: `weftdb-memory` p50 0.857 ms vs `duckdb-double` 1.138 ms (weftdb-memory 1.3× faster).
- `block-tiny` interpolate: `weftdb-stored` p50 4.726 ms vs `duckdb-decimal` 18.558 ms (weftdb-stored 3.9× faster).
- `block-tiny` interpolate: `weftdb-stored-f64` p50 1.516 ms vs `duckdb-decimal` 18.558 ms (weftdb-stored-f64 12.2× faster).
- `block-tiny` interpolate: `weftdb-stored-bulk` p50 4.667 ms vs `duckdb-decimal` 18.558 ms (weftdb-stored-bulk 4.0× faster).
- `block-tiny` downsample: `weftdb-stored-bulk` p50 0.996 ms vs `duckdb-double` 1.138 ms (weftdb-stored-bulk 1.1× faster).
- `mcar-small` downsample: `weftdb-memory` p50 1.745 ms vs `duckdb-decimal` 1.975 ms (weftdb-memory 1.1× faster).
- `mcar-small` interpolate: `weftdb-stored-f64` p50 8.396 ms vs `duckdb-decimal` 23.617 ms (weftdb-stored-f64 2.8× faster).
- `block-small` interpolate: `weftdb-memory` p50 19.509 ms vs `duckdb-double` 21.747 ms (weftdb-memory 1.1× faster).
- `block-small` downsample: `weftdb-memory` p50 1.406 ms vs `duckdb-double` 1.799 ms (weftdb-memory 1.3× faster).
- `block-small` interpolate: `weftdb-stored-f64` p50 6.134 ms vs `duckdb-double` 21.747 ms (weftdb-stored-f64 3.5× faster).
- `uci-tiny` interpolate: `weftdb-memory` p50 1.038 ms vs `duckdb-double` 8.425 ms (weftdb-memory 8.1× faster).
- `uci-tiny` downsample: `weftdb-memory` p50 0.082 ms vs `duckdb-double` 0.900 ms (weftdb-memory 11.0× faster).
- `uci-tiny` interpolate: `weftdb-stored` p50 1.386 ms vs `duckdb-double` 8.425 ms (weftdb-stored 6.1× faster).
- `uci-tiny` downsample: `weftdb-stored` p50 0.306 ms vs `duckdb-double` 0.900 ms (weftdb-stored 2.9× faster).
- `uci-tiny` interpolate: `weftdb-stored-f64` p50 0.553 ms vs `duckdb-double` 8.425 ms (weftdb-stored-f64 15.2× faster).
- `uci-tiny` interpolate: `weftdb-stored-bulk` p50 1.146 ms vs `duckdb-double` 8.425 ms (weftdb-stored-bulk 7.4× faster).
- `uci-tiny` downsample: `weftdb-stored-bulk` p50 0.269 ms vs `duckdb-double` 0.900 ms (weftdb-stored-bulk 3.3× faster).
- `uci-small` interpolate: `weftdb-memory` p50 20.481 ms vs `duckdb-double` 25.714 ms (weftdb-memory 1.3× faster).
- `uci-small` interpolate: `weftdb-stored-f64` p50 9.612 ms vs `duckdb-double` 25.714 ms (weftdb-stored-f64 2.7× faster).
- `uci-small` interpolate: `weftdb-stored-bulk` p50 23.474 ms vs `duckdb-double` 25.714 ms (weftdb-stored-bulk 1.1× faster).
- `uci-medium` interpolate: `weftdb-stored-f64` p50 58.268 ms vs `duckdb-double` 81.883 ms (weftdb-stored-f64 1.4× faster).

### Where WeftDB loses

- `mcar-tiny` load: `weftdb-stored` p50 211.690 ms vs `duckdb-double` 19.545 ms (duckdb-double 10.8× faster).
- `mcar-tiny` load: `weftdb-stored-f64` p50 200.596 ms vs `duckdb-double` 19.545 ms (duckdb-double 10.3× faster).
- `mcar-tiny` load: `weftdb-stored-bulk` p50 183.503 ms vs `duckdb-double` 19.545 ms (duckdb-double 9.4× faster).
- `block-tiny` load: `weftdb-stored` p50 200.161 ms vs `duckdb-double` 21.205 ms (duckdb-double 9.4× faster).
- `block-tiny` load: `weftdb-stored-f64` p50 203.540 ms vs `duckdb-double` 21.205 ms (duckdb-double 9.6× faster).
- `block-tiny` load: `weftdb-stored-bulk` p50 191.349 ms vs `duckdb-double` 21.205 ms (duckdb-double 9.0× faster).
- `mcar-small` load: `weftdb-stored` p50 927.833 ms vs `duckdb-double` 57.071 ms (duckdb-double 16.3× faster).
- `mcar-small` interpolate: `weftdb-stored` p50 29.203 ms vs `duckdb-decimal` 23.617 ms (duckdb-decimal 1.2× faster).
- `mcar-small` downsample: `weftdb-stored` p50 4.334 ms vs `duckdb-decimal` 1.975 ms (duckdb-decimal 2.2× faster).
- `mcar-small` load: `weftdb-stored-f64` p50 901.611 ms vs `duckdb-double` 57.071 ms (duckdb-double 15.8× faster).
- `mcar-small` load: `weftdb-stored-bulk` p50 297.364 ms vs `duckdb-double` 57.071 ms (duckdb-double 5.2× faster).
- `mcar-small` downsample: `weftdb-stored-bulk` p50 4.827 ms vs `duckdb-decimal` 1.975 ms (duckdb-decimal 2.4× faster).
- `block-small` load: `weftdb-stored` p50 766.492 ms vs `duckdb-double` 49.848 ms (duckdb-double 15.4× faster).
- `block-small` interpolate: `weftdb-stored` p50 24.026 ms vs `duckdb-double` 21.747 ms (duckdb-double 1.1× faster).
- `block-small` downsample: `weftdb-stored` p50 3.217 ms vs `duckdb-double` 1.799 ms (duckdb-double 1.8× faster).
- `block-small` load: `weftdb-stored-f64` p50 766.026 ms vs `duckdb-double` 49.848 ms (duckdb-double 15.4× faster).
- `block-small` load: `weftdb-stored-bulk` p50 267.073 ms vs `duckdb-double` 49.848 ms (duckdb-double 5.4× faster).
- `block-small` downsample: `weftdb-stored-bulk` p50 3.577 ms vs `duckdb-double` 1.799 ms (duckdb-double 2.0× faster).
- `mcar-medium` interpolate: `weftdb-memory` p50 142.803 ms vs `duckdb-double` 50.859 ms (duckdb-double 2.8× faster).
- `mcar-medium` downsample: `weftdb-memory` p50 7.919 ms vs `duckdb-double` 5.371 ms (duckdb-double 1.5× faster).
- `mcar-medium` load: `weftdb-stored` p50 5555.821 ms vs `duckdb-double` 204.262 ms (duckdb-double 27.2× faster).
- `mcar-medium` interpolate: `weftdb-stored` p50 198.326 ms vs `duckdb-double` 50.859 ms (duckdb-double 3.9× faster).
- `mcar-medium` downsample: `weftdb-stored` p50 17.879 ms vs `duckdb-double` 5.371 ms (duckdb-double 3.3× faster).
- `mcar-medium` load: `weftdb-stored-f64` p50 5535.440 ms vs `duckdb-double` 204.262 ms (duckdb-double 27.1× faster).
- `mcar-medium` interpolate: `weftdb-stored-f64` p50 67.711 ms vs `duckdb-double` 50.859 ms (duckdb-double 1.3× faster).
- `mcar-medium` load: `weftdb-stored-bulk` p50 964.127 ms vs `duckdb-double` 204.262 ms (duckdb-double 4.7× faster).
- `mcar-medium` interpolate: `weftdb-stored-bulk` p50 210.524 ms vs `duckdb-double` 50.859 ms (duckdb-double 4.1× faster).
- `mcar-medium` downsample: `weftdb-stored-bulk` p50 44.466 ms vs `duckdb-double` 5.371 ms (duckdb-double 8.3× faster).
- `block-medium` interpolate: `weftdb-memory` p50 112.244 ms vs `duckdb-double` 42.999 ms (duckdb-double 2.6× faster).
- `block-medium` downsample: `weftdb-memory` p50 5.774 ms vs `duckdb-double` 4.148 ms (duckdb-double 1.4× faster).
- `block-medium` load: `weftdb-stored` p50 4246.923 ms vs `duckdb-double` 180.401 ms (duckdb-double 23.5× faster).
- `block-medium` interpolate: `weftdb-stored` p50 152.038 ms vs `duckdb-double` 42.999 ms (duckdb-double 3.5× faster).
- `block-medium` downsample: `weftdb-stored` p50 13.111 ms vs `duckdb-double` 4.148 ms (duckdb-double 3.2× faster).
- `block-medium` load: `weftdb-stored-f64` p50 4272.610 ms vs `duckdb-double` 180.401 ms (duckdb-double 23.7× faster).
- `block-medium` load: `weftdb-stored-bulk` p50 802.981 ms vs `duckdb-double` 180.401 ms (duckdb-double 4.5× faster).
- `block-medium` interpolate: `weftdb-stored-bulk` p50 149.209 ms vs `duckdb-double` 42.999 ms (duckdb-double 3.5× faster).
- `block-medium` downsample: `weftdb-stored-bulk` p50 35.164 ms vs `duckdb-double` 4.148 ms (duckdb-double 8.5× faster).
- `uci-tiny` load: `weftdb-stored` p50 214.496 ms vs `duckdb-decimal` 23.562 ms (duckdb-decimal 9.1× faster).
- `uci-tiny` load: `weftdb-stored-f64` p50 225.491 ms vs `duckdb-decimal` 23.562 ms (duckdb-decimal 9.6× faster).
- `uci-tiny` load: `weftdb-stored-bulk` p50 190.931 ms vs `duckdb-decimal` 23.562 ms (duckdb-decimal 8.1× faster).
- `uci-small` load: `weftdb-stored` p50 907.852 ms vs `duckdb-double` 53.643 ms (duckdb-double 16.9× faster).
- `uci-small` downsample: `weftdb-stored` p50 3.040 ms vs `duckdb-double` 2.184 ms (duckdb-double 1.4× faster).
- `uci-small` load: `weftdb-stored-f64` p50 915.667 ms vs `duckdb-double` 53.643 ms (duckdb-double 17.1× faster).
- `uci-small` load: `weftdb-stored-bulk` p50 297.612 ms vs `duckdb-double` 53.643 ms (duckdb-double 5.5× faster).
- `uci-small` downsample: `weftdb-stored-bulk` p50 3.753 ms vs `duckdb-double` 2.184 ms (duckdb-double 1.7× faster).
- `uci-medium` interpolate: `weftdb-memory` p50 140.672 ms vs `duckdb-double` 81.883 ms (duckdb-double 1.7× faster).
- `uci-medium` downsample: `weftdb-memory` p50 15.807 ms vs `duckdb-decimal` 6.275 ms (duckdb-decimal 2.5× faster).
- `uci-medium` load: `weftdb-stored` p50 5294.161 ms vs `duckdb-double` 196.959 ms (duckdb-double 26.9× faster).
- `uci-medium` interpolate: `weftdb-stored` p50 191.404 ms vs `duckdb-double` 81.883 ms (duckdb-double 2.3× faster).
- `uci-medium` downsample: `weftdb-stored` p50 21.966 ms vs `duckdb-decimal` 6.275 ms (duckdb-decimal 3.5× faster).
- `uci-medium` load: `weftdb-stored-f64` p50 5303.131 ms vs `duckdb-double` 196.959 ms (duckdb-double 26.9× faster).
- `uci-medium` load: `weftdb-stored-bulk` p50 923.009 ms vs `duckdb-double` 196.959 ms (duckdb-double 4.7× faster).
- `uci-medium` interpolate: `weftdb-stored-bulk` p50 169.015 ms vs `duckdb-double` 81.883 ms (duckdb-double 2.1× faster).
- `uci-medium` downsample: `weftdb-stored-bulk` p50 35.885 ms vs `duckdb-decimal` 6.275 ms (duckdb-decimal 5.7× faster).

### Within noise

- `mcar-tiny` downsample: `weftdb-memory` p50 0.972 ms vs `duckdb-double` 1.014 ms (intervals overlap).
- `mcar-tiny` downsample: `weftdb-stored` p50 1.231 ms vs `duckdb-double` 1.014 ms (intervals overlap).
- `mcar-tiny` downsample: `weftdb-stored-bulk` p50 1.286 ms vs `duckdb-double` 1.014 ms (intervals overlap).
- `block-tiny` downsample: `weftdb-stored` p50 1.206 ms vs `duckdb-double` 1.138 ms (intervals overlap).
- `mcar-small` interpolate: `weftdb-memory` p50 24.087 ms vs `duckdb-decimal` 23.617 ms (intervals overlap).
- `mcar-small` interpolate: `weftdb-stored-bulk` p50 27.648 ms vs `duckdb-decimal` 23.617 ms (intervals overlap).
- `block-small` interpolate: `weftdb-stored-bulk` p50 22.696 ms vs `duckdb-double` 21.747 ms (intervals overlap).
- `block-medium` interpolate: `weftdb-stored-f64` p50 45.520 ms vs `duckdb-double` 42.999 ms (intervals overlap).
- `uci-small` downsample: `weftdb-memory` p50 1.835 ms vs `duckdb-double` 2.184 ms (intervals overlap).
- `uci-small` interpolate: `weftdb-stored` p50 28.044 ms vs `duckdb-double` 25.714 ms (intervals overlap).

### Not tested

- Gap fill, point and range reads: measured by `weft-bench` against WeftDB's own baselines, not against another engine.
- Spline (quadratic, cubic) reconstruction: DuckDB has no spline, so only linear interpolation is compared.
- Concurrent ingest and query, many series, and long-running behaviour: covered by the soak harness, not by this report.
- QuestDB, InfluxDB 3 Core, TimescaleDB and ClickHouse: no adapter yet. Enterprise editions of QuestDB and InfluxDB 3 are excluded by their licences (see WILLINGNESS-TO-PAY.md).
- kdb+ and AVEVA PI: their licences bar benchmarking or publishing results without written consent.
- GPU backends: every arm runs splimes' automatic backend selection; GPU-specific tuning is a separate report.

### Known limitations

- DuckDB has no interpolation function. Its arm is hand-written SQL (two `ASOF JOIN`s and the first/last segment extended), which is what a DuckDB user would write, and it is gated against WeftDB's grid.
- WeftDB has no last-observation-carried-forward fill on stored data in 1.0, so no LOCF comparison is made.
- Every dataset is a single series; the grid is fixed per dataset, so input size grows with scale while output size does not.
- Load layouts differ by arm: `weftdb-stored` and `weftdb-stored-f64` seal 1,024-row durable segments (the layout the stored read path is measured on), `weftdb-stored-bulk` seals one segment per load, and DuckDB does one bulk append and one CHECKPOINT. `weftdb-stored-bulk` is the like-for-like load comparison.
- `duckdb-decimal` stores the knots exactly as `DECIMAL(18, s)`, but its interpolation line is computed in `DOUBLE` (DuckDB's `DECIMAL × DOUBLE` is `DOUBLE`), as is WeftDB's (splimes computes in `f64`). Neither engine interpolates in exact decimal arithmetic.

### Configuration caveats

- Hardware: AMD Ryzen 9 7950X3D 16-Core Processor (16 physical / 16 logical cores, 125 GiB memory). The working directory was on a hdd btrfs disk at /mnt/deepmem.
- Both engines wrote their stores to `/mnt/weftbench/tmp` (`TMPDIR`), which may be a different disk from the working directory.
- Some results have only 5 reps, so their intervals are wide and fewer comparisons can separate from noise.

### Reproduction

- Check out commit `9903465fe0a3f286787957da7fa135d23ac04a72`.
- Build: `cargo build --release --manifest-path weft-xbench/Cargo.toml` (DuckDB compiles from C++; put `CARGO_TARGET_DIR` on a fast local disk).
- Run: `TMPDIR=<scratch on the disk under test> weft-xbench --scales tiny,small,medium --reps 10 --load-reps 5 --uci /mnt/weftbench/data/uci-household-power/household_power_consumption.txt --out /mnt/weftbench/reports/evi15-r2`.
- Render: `weft-xbench render <xbench.json> --out <dir>`. The synthetic datasets are regenerated from seed 1592649491.

