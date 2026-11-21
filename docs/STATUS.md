# Status

Updated 2026-11-21 05:51 UTC.

| Item | Value |
|---|---|
| Best practice score | 1.14 s (commit 4ea55a8) |
| Board leader | 0.65 s |
| Target (leader minus 10 percent) | 0.585 s |
| Main | solver code of 4ea55a8 (allocator huge pages) |

## Work in progress

| Branch | Change | State |
|---|---|---|
| opt/parse2 | event parse stage redesign (53 percent of runtime) | building |
| opt/sessions2 | sessions and output rewrite (25 percent of runtime) | building, gates running |
| opt/people2 | linked-ID build rewrite (29 percent, runs beside parsing) | building |
| opt/nocomments | remove code comments from the shipped files with a parser | building |
| opt/parse-fix | inline timestamp parsing, word-at-a-time prices, review fix | gates pass; slower than the best scored code in a pinned 9-round bench (median 1123 ms against 1078 ms); not submitted |
| opt/cand1 | main plus opt/writer and opt/dedupbuf | slower than the best scored code in a pinned 7-round bench (median 1420 ms against 1094 ms); not submitted |
| opt/coldread, opt/prefault | Linux read and pre-fault changes | no measured gain; parked |
