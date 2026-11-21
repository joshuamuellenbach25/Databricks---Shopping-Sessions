# Status

Updated 2026-11-21 05:23 UTC.

| Item | Value |
|---|---|
| Best practice score | 1.14 s (commit 4ea55a8) |
| Board leader | 0.65 s |
| Target (leader minus 10 percent) | 0.585 s |
| Main | solver code of 4ea55a8 (allocator huge pages), scored 1.14 s |

## Work in progress

| Branch | Change | State |
|---|---|---|
| opt/parse2 | event parse stage redesign (53 percent of runtime) | building |
| opt/sessions2 | sessions and output rewrite (25 percent of runtime) | building |
| opt/people2 | linked-ID build rewrite (29 percent, runs beside parsing) | building |
| opt/cand1 | main plus opt/writer and opt/dedupbuf | gates pass, Linux dev answer matches; waits for review fixes |
| opt/writer | write Parquet columns from reused buffers | review found 2 items; fix commit on opt/writer-fix (0501848), Codex re-review running |
| opt/dedupbuf | reuse dedup output buffers | review triage in progress |
| opt/parse | inline timestamp parsing, word-at-a-time prices | review found 1 item; fix commit on opt/parse-fix (27faa5d), Codex re-review running |
| opt/coldread, opt/prefault | Linux read and pre-fault changes | no measured gain; parked |
