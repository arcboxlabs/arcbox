# YYYY-MM-DD — <the question, as a sentence>

- Type: experiment
- Area: <crates / subsystem>
- Outcome: <ADR NNNN | commit hashes | rejected | open>
- Probes: <tests/bench/<name>/ or e2e target; nothing that lives only on one machine>
- Host / guest: <hardware, macOS, load during runs; guest shape and the
  kernel facts that affect the numbers>

## Question

What was unknown, and what answer would change the code. One paragraph.

## Hypotheses

Numbered. Each one falsifiable by something below.

## Method

Configurations (knobs, builds), what was run per configuration, how many
times, in what order. Enough for a rerun.

## Results

Tables. Same units and conditions per row; load noted when it varied.

## Findings

Numbered claims the results support, each with the mechanism or
"mechanism unknown". Say which hypotheses died.

## Decisions taken / open

Link the ADR or commits; list what is still unanswered.
