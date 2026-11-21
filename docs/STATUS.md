# Results

The track timed each submission's container on about 20 million practice events; lower wins. The team made 11 scored submissions between November 20 and 21, 2026, and entered the fastest, 0.85 s, as its final.

| # | Receipt | Commit | Change | Time |
|---|---|---|---|---|
| 1 | BB-1C70D0F2 | 86307b4 | First scored build: the records, tokenizer, fields and memory changes merged | 1.18 s |
| 2 | BB-A9E7D6D8 | b4c44e1 | Faster process exit | 1.15 s |
| 3 | BB-BDA65F4A | 4ea55a8 | Huge pages for the allocator | 1.14 s |
| 4 | BB-C3C3BBDE | 79a758b | Event parse stage redesign | 1.04 s |
| 5 | BB-4DFDD35A | 5cd5648 | Earlier release of memory and input mappings | 1.04 s |
| 6 | BB-FEAC4A51 | 8643225 | Sessions and output rewrite | 0.96 s |
| 7 | BB-E8BAC516 | a67654b | Faster event deduplication and click filing | 0.92 s |
| 8 | BB-E62309C7 | 7c88fa7 | Sessions and output change whose local gain was Windows file work; not merged | 0.95 s |
| 9 | BB-3CA3694B | 919c060 | Faster field decoding and record filing | 0.91 s |
| 10 | BB-0BEA6B7B | deae350 | Block tokenizer for events and a person table built in parts | 0.90 s |
| 11 | BB-E410AFDA | 8981d63 | Faster sessions phase and earlier release of the link pairs; the final entry | 0.85 s |

## Final-scale check

Before the final, commit 8981d63 ran on 60 million generated events: it exited cleanly with a peak working set of 8.43 GiB, and both output tables matched the 0.90 s build's output exactly.

## Checks before each submission

Submissions 4 to 11 each passed the fixture cases, the published dev set and seeded differential tests against the Python reference before they were submitted.

## Branches

The opt/* branches hold the experiments, candidate builds and merge staging behind these submissions; the table above names the commits that were scored.
