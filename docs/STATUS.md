# Status

Updated 2026-11-21 09:52 UTC.

| Item | Value |
|---|---|
| Best practice score | 0.96 s (commit 8643225, sessions and output rewrite; entered in the final) |
| Board leader | 0.36 s |
| Target (leader minus 10 percent) | 0.324 s |
| Main | solver code of 8643225 (parse stage redesign plus the sessions and output rewrite, no code comments); the opt/memexit changes are not in it, since they scored the same 1.04 s |

## Work in progress

| Branch | Change | State |
|---|---|---|
| opt/parse2 | event parse stage redesign (53 percent of runtime), comments stripped | gates pass (79a758b); pinned bench median 1049 ms against 1121 ms for the best scored code; scored 1.04 s (receipt BB-C3C3BBDE); merged into main |
| opt/people2 | linked-ID build in parts on several threads | b88353f; gates pass; people phase 0.227 s to 0.020 s in a pinned 16-round bench, but that phase runs beside the events parse, so the whole run did not get faster (see opt/cand2); parked |
| opt/cand2 | main plus the opt/people2 change, comments stripped | 0d40115; gates pass; Codex found no output defect; pinned bench median 1067 ms against 1070 ms for main, faster in 6 of 15 low-load rounds; not submitted |
| opt/main-fix1 | quoted CPU pinning in the volume runner, no comment lines in Cargo.lock | d05a8d9; gates pass with locked cargo (43 tests, 36/36, dev MATCH, 6/6); every cargo command locked; Codex re-review clean; merged into main |
| opt/memexit | release memory and input mappings early to cut page zeroing and exit teardown (about 0.4 s of each local run falls after the last timed phase) | 5cd5648; gates pass (45 tests, 36/36, dev MATCH, 6/6); time after the last phase 339 ms to 91 ms; pinned bench median 901 ms against 1104.5 ms for the 1.04 s build, faster in 11 of 12 low-load rounds; Codex review found nothing to fix; scored 1.04 s (receipt BB-4DFDD35A): the local gain was Windows exit time and did not reach the Linux scorer; merged into main, then left out when main took the 0.96 s build's solver files |
| opt/directio | Linux input reads that skip the page cache when the data is not resident, overlapped with parsing | ee748b6 on the old main; gates pass on Windows (44 tests, 36/36, dev MATCH, 6/6), where only the fallback path runs; Codex re-check left 2 test-only items; the Linux code has not been compiled yet (no Linux target or Docker here) |
| opt/sessions2 | sessions and output rewrite (25 percent of runtime) | a90c101 on opt/cand1; sessions and output 0.174 s to 0.097 s, lower in 16 of 16 rounds; gates pass; Codex confirmed 3 items (stale parts when an output folder is reused with fewer threads, empty parts left by a failed run, added comments); fix 9e9c9f3 stages output parts and publishes them after a whole run; Codex re-check running |
| opt/cand5 | the 0.96 s main plus the direct-read change from opt/cand4 | 3a54869; gates pass (65 tests, 36/36, dev MATCH, 6/6); Codex review found nothing to fix; the Linux code type-checks against libc 0.2.191 with its cfgs forced on, but has not been built or run on Linux; the native bench cannot measure it |
| opt/parse3 | events parse throughput (block size of the record flush) | cc8936e; gates pass; about 2 percent faster in fast-mode rounds, under the 5 percent floor; stopped |
| opt/parse4 | events-only tokenizer and a cheaper first touch of input pages, on the 0.96 s main | building |
| opt/dedup2 | events dedup and clicks on the current main (24 percent of runtime) | building |
| opt/sessions3 | sessions and output on the 0.96 s main (about 21 percent of runtime) | building |
| opt/cand4 | the 1.04 s main plus the opt/directio change, comments stripped | 51d0233; gates pass on Windows (55 tests, 36/36, dev MATCH, 6/6); Codex confirmed 1 item (overlay mounts refuse the read probe; fixed: unknown residency now reads directly), 0 left; the Linux code is not compiled here; needs a port onto the 0.96 s main |
| opt/cand3 | main plus only the opt/sessions2 change, comments stripped | 0890fe7; gates pass (48 tests, 36/36, dev MATCH, 6/6); reference matched in 228 of 228 runs; Codex confirmed 5 items, none in a single fresh run (reused output folder, failed rerun, per-worker category copy, open file handles, a 2^31 event limit); fix 8643225 (output published only after a whole run, writer text no longer leaked); bench median 1738 ms against 1914 ms, 16 of 25 rounds; scored 0.96 s (receipt BB-FEAC4A51); merged into main |
| opt/nocomments | remove code comments from the shipped files with a parser | superseded: the parse2 merge removed every comment from main (parser check: 0 left) |
| opt/parse-fix | inline timestamp parsing, word-at-a-time prices, review fix | gates pass; slower than the best scored code in a pinned 9-round bench (median 1123 ms against 1078 ms); not submitted |
| opt/cand1 | main plus opt/writer and opt/dedupbuf | slower than the best scored code in a pinned 7-round bench (median 1420 ms against 1094 ms); not submitted |
| opt/coldread, opt/prefault | Linux read and pre-fault changes | no measured gain; parked |
