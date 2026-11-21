# Status

Updated 2026-11-21 06:46 UTC.

| Item | Value |
|---|---|
| Best practice score | 1.14 s (commit 4ea55a8) |
| Board leader | 0.59 s |
| Target (leader minus 10 percent) | 0.531 s |
| Main | solver code of 4ea55a8 (allocator huge pages) |

## Work in progress

| Branch | Change | State |
|---|---|---|
| opt/parse2 | event parse stage redesign (53 percent of runtime), comments stripped | gates pass (79a758b); pinned bench median 1049 ms against 1121 ms for the best scored code; submitted, being scored |
| opt/people2 | linked-ID build in parts on several threads | first commit b88353f; gates and Codex review in progress |
| opt/directio | Linux input reads that skip the page cache when the data is not resident, overlapped with parsing | building |
| opt/sessions2 | sessions and output rewrite (25 percent of runtime) | building, gates running |
| opt/people2 | linked-ID build rewrite (29 percent, runs beside parsing) | building |
| opt/nocomments | remove code comments from the shipped files with a parser | committed (159eaa3); Codex review running |
| opt/parse-fix | inline timestamp parsing, word-at-a-time prices, review fix | gates pass; slower than the best scored code in a pinned 9-round bench (median 1123 ms against 1078 ms); not submitted |
| opt/cand1 | main plus opt/writer and opt/dedupbuf | slower than the best scored code in a pinned 7-round bench (median 1420 ms against 1094 ms); not submitted |
| opt/coldread, opt/prefault | Linux read and pre-fault changes | no measured gain; parked |
