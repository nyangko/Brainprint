| arm | session USD mean [per session] | B−N | BASE | SETUP(ToolSearch) | BP_RESULT | NATIVE_RESULT | OUTPUT | cache-write / cache-read tok | BP calls / KB | native calls / KB | ToolSearch tok | grades |
|---|---|---|---|---|---|---|---|---|---|---|---|---|
| N | 0.556 [0.54255, 0.61848, 0.54125, 0.52105] | +0.000 | 0.108 | 0.000 | 0.000 | 0.208 | 0.201 | 31012 / 531591 | 0 / 0.0 | 15.75 / 33.8 | 0 | 16/16 |
| A | 1.000 [1.04836, 0.88503, 1.16465, 0.90141] | +0.444 | 0.128 | 0.102 | 0.380 | 0.156 | 0.196 | 73720 / 1067840 | 13.25 / 68.7 | 10.25 / 23.3 | 8712 | 15/16 |
| B | 0.878 [0.78273, 1.07038, 0.81742, 0.8417] | +0.322 | 0.127 | 0.075 | 0.296 | 0.160 | 0.184 | 63740 / 919265 | 10.5 / 54.8 | 10.75 / 25.0 | 6453 | 16/16 |
| C | 0.858 [0.77062, 0.94511] | +0.302 | 0.133 | 0.110 | 0.255 | 0.140 | 0.181 | 60532 / 959444 | 8.5 / 46.7 | 11.5 / 24.1 | 10096 | 8/8 |
| D | 1.046 [0.96315, 1.12874] | +0.490 | 0.144 | 0.009 | 0.481 | 0.148 | 0.222 | 72350 / 1226505 | 16 / 86.7 | 9.5 / 23.8 | 659 | 8/8 |
| E | 0.901 [1.02522, 0.77686] | +0.345 | 0.128 | 0.122 | 0.284 | 0.153 | 0.178 | 62709 / 1107755 | 9 / 46.8 | 12 / 25.7 | 10620 | 8/8 |

per-task USD mean:
  N {'T1': 0.1706, 'T5': 0.1196, 'T6': 0.1594, 'T7': 0.1062}
  A {'T1': 0.2369, 'T5': 0.2108, 'T6': 0.3092, 'T7': 0.243}
  B {'T1': 0.2731, 'T5': 0.1963, 'T6': 0.1935, 'T7': 0.2152}
  C {'T1': 0.2178, 'T5': 0.1496, 'T6': 0.2394, 'T7': 0.2511}
  D {'T1': 0.237, 'T5': 0.2207, 'T6': 0.3029, 'T7': 0.2853}
  E {'T1': 0.2244, 'T5': 0.232, 'T6': 0.2131, 'T7': 0.2315}

proxy call log (B-E): calls / invalid / lookups / expands / result KB per session
  B: calls 10.5  invalid 0  lookups 0  expands 0  errors 5  result_KB 92.1  tools {'brainprint.context': 21, 'brainprint.inspect': 8, 'brainprint.find': 8, 'brainprint.relations': 5}
  C: calls 8.5  invalid 0  lookups 0  expands 0  errors 3  result_KB 84.0  tools {'brainprint.inspect': 3, 'brainprint.relations_direct': 2, 'brainprint.context_change': 4, 'brainprint.context_rules': 2, 'brainprint.context_work_items': 2, 'brainprint.context_resume': 3, 'brainprint.find_target': 1}
  D: calls 16.0  invalid 2  lookups 7  expands 0  errors 4  result_KB 123.9  tools {'brainprint.contract': 7, 'brainprint.call': 25}
  E: calls 9.0  invalid 0  lookups 0  expands 0  errors 2  result_KB 46.8  tools {'brainprint.context': 11, 'brainprint.inspect': 4, 'brainprint.relations': 2, 'brainprint.find': 1}

auto_class (native after BP):
  A {'FALLBACK_BOUNDED': 8, 'FALLBACK_ERROR': 3, 'FALLBACK_PARTIAL': 12, 'NOT_DELIVERED': 18} native_before_bp 0
  B {'FALLBACK_BOUNDED': 10, 'FALLBACK_PARTIAL': 17, 'NOT_DELIVERED': 16} native_before_bp 0
  C {'FALLBACK_BOUNDED': 12, 'NOT_DELIVERED': 11} native_before_bp 0
  D {'FALLBACK_BOUNDED': 4, 'FALLBACK_PARTIAL': 8, 'NOT_DELIVERED': 7} native_before_bp 0
  E {'FALLBACK_BOUNDED': 9, 'FALLBACK_PARTIAL': 11, 'NOT_DELIVERED': 4} native_before_bp 0
