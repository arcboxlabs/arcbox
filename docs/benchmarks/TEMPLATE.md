# <subject> — measured performance and known limits

Benchmark record. One file per subject, kept current: the method stays
stable so rows are comparable; results append with a date; the analysis is
rewritten when a new result changes the picture.

## Method

What is measured, with which probe (`tests/bench/<name>/` or an e2e
target), against which backend and shape, and what "same context" means
for this subject (same day, same load band, same guest kernel, …). List
what must NOT be compared (e.g. cache-hot native vs FUSE).

## Results

| date | commit / build | config | metric 1 | metric 2 | load | notes |
|---|---|---|---|---|---|---|

Newest rows at the bottom. A row without a load column is not a result.

## Analysis

What the numbers mean, what the ceiling is, and what was ruled out. Point
at the experiment entry that established each claim rather than repeating
its tables.

## Known limits

The settled facts a future change must not rediscover, each with the row
or entry that established it.
