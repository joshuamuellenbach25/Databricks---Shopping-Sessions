# Status

Updated 2026-11-21 05:13 UTC.

| Item | Value |
|---|---|
| Best practice score | 1.15 s (commit b4c44e1) |
| Board leader | 0.65 s |
| Target (leader minus 10 percent) | 0.585 s |
| Main | solver code of 4ea55a8 (allocator huge pages), being scored |

## Work in progress

| Branch | Change | State |
|---|---|---|
| opt/parse2 | event parse stage redesign (53 percent of runtime) | building |
| opt/sessions2 | sessions and output rewrite (25 percent of runtime) | building |
| opt/people2 | linked-ID build rewrite (29 percent, runs beside parsing) | building |
| opt/cand1 | main plus opt/writer and opt/dedupbuf | gates pass, Linux dev answer matches; waits for review fixes |
| opt/writer | write Parquet columns from reused buffers | review found 2 items; fixes in progress |
| opt/dedupbuf | reuse dedup output buffers | review triage in progress |
| opt/parse | inline timestamp parsing, word-at-a-time prices | review found 1 item; fix in progress |
| opt/coldread, opt/prefault | Linux read and pre-fault changes | no measured gain; parked |
