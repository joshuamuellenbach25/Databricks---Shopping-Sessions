# Status

Updated 2026-11-21 04:39 UTC.

| Item | Value |
|---|---|
| Best practice score | 1.15 s (commit b4c44e1) |
| Board leader | 0.65 s |
| Target (leader minus 10 percent) | 0.585 s |
| Main | 4ea55a8 (allocator huge pages) |

## Work in progress

| Branch | Change | State |
|---|---|---|
| opt/coldread | read inputs with pread into reused buffers on Linux | gates pass, dev answer matches on Linux, audit running |
| opt/prefault | pre-fault parse record vectors on Linux | gates pass, dev answer matches on Linux, audit running |
| opt/parse | faster event parse loop | building |
| opt/dedupbuf | reuse dedup output buffers | building |
| opt/writer | write Parquet columns from reused buffers | first commit 02883c1, gates pending |
| opt/tsonce | parse each timestamp once | building |
