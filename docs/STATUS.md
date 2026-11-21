# Status

Updated 2026-11-21 04:58 UTC.

| Item | Value |
|---|---|
| Best practice score | 1.15 s (commit b4c44e1) |
| Board leader | 0.65 s |
| Target (leader minus 10 percent) | 0.585 s |
| Main | solver code of 4ea55a8 (allocator huge pages) |

## Work in progress

| Branch | Change | State |
|---|---|---|
| opt/cand1 | main plus opt/writer and opt/dedupbuf | four gates pass, Linux dev answer matches, faster than main in 3 of 5 local rounds; waits for the Codex reviews |
| opt/writer | write Parquet columns from reused buffers | gates pass, Linux dev answer matches, Codex review running |
| opt/dedupbuf | reuse dedup output buffers | gates pass, Linux dev answer matches, Codex review running |
| opt/coldread | read inputs with pread into reused buffers on Linux | gates pass, Linux dev answer matches, audit and Codex review running |
| opt/prefault | pre-fault parse record vectors on Linux | gates pass, Linux dev answer matches, audit and Codex review running |
| opt/parse | inline timestamp parsing, word-at-a-time prices | gates pass, Codex review running |
| opt/parse2 | parse stage redesign | building |
| opt/tsonce | parse each timestamp once | building |
