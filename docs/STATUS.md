# Status

Updated 2026-11-21 07:16 UTC.

| Item | Value |
|---|---|
| Best practice score | 1.04 s (commit 79a758b, parse stage redesign) |
| Board leader | 0.59 s |
| Target (leader minus 10 percent) | 0.531 s |
| Main | solver code of 79a758b (parse stage redesign, no code comments) |

## Work in progress

| Branch | Change | State |
|---|---|---|
| opt/parse2 | event parse stage redesign (53 percent of runtime), comments stripped | gates pass (79a758b); pinned bench median 1049 ms against 1121 ms for the best scored code; scored 1.04 s (receipt BB-C3C3BBDE); merged into main |
| opt/people2 | linked-ID build in parts on several threads | b88353f; gates pass; people phase 0.227 s to 0.020 s in a pinned 16-round bench, but that phase runs beside the events parse, so the whole run did not get faster (see opt/cand2); parked |
| opt/cand2 | main plus the opt/people2 change, comments stripped | 0d40115; gates pass; Codex found no output defect; pinned bench median 1067 ms against 1070 ms for main, faster in 6 of 15 low-load rounds; not submitted |
| opt/main-fix1 | quoted CPU pinning in the volume runner, no comment lines in Cargo.lock | 566828a; gates pass with locked cargo (43 tests, 36/36, dev MATCH, 6/6); Codex re-review running |
| opt/memexit | release memory and input mappings early to cut page zeroing and exit teardown (about 0.4 s of each local run falls after the last timed phase) | building |
| opt/directio | Linux input reads that skip the page cache when the data is not resident, overlapped with parsing | 480b924 on the old main; Codex review running; needs a Linux bench |
| opt/sessions2 | sessions and output rewrite (25 percent of runtime) | building, gates running |
| opt/nocomments | remove code comments from the shipped files with a parser | superseded: the parse2 merge removed every comment from main (parser check: 0 left) |
| opt/parse-fix | inline timestamp parsing, word-at-a-time prices, review fix | gates pass; slower than the best scored code in a pinned 9-round bench (median 1123 ms against 1078 ms); not submitted |
| opt/cand1 | main plus opt/writer and opt/dedupbuf | slower than the best scored code in a pinned 7-round bench (median 1420 ms against 1094 ms); not submitted |
| opt/coldread, opt/prefault | Linux read and pre-fault changes | no measured gain; parked |
