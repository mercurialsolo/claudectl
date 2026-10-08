# Pricing source of record

Fetched 2026-10-08 from https://platform.claude.com/docs/en/about-claude/pricing
(and https://platform.claude.com/docs/en/docs/build-with-claude/prompt-caching
for the multipliers). All USD per million tokens. These rows are the ONLY
source for the constants in `crates/claudectl-core/src/models.rs`.

| Model            | input | 5m write | 1h write | cache read | output |
|------------------|-------|----------|----------|------------|--------|
| Claude Fable 5.1 | 10    | 12.50    | 20       | 0.25       | 50     |
| Claude Fable 5   | 10    | 12.50    | 20       | 1.00       | 50     |
| Claude Opus 5.5  | 4     | 5.00     | 8        | 0.20       | 20     |
| Claude Opus 5    | 5     | 6.25     | 10       | 0.50       | 25     |
| Claude Opus 4.8  | 5     | 6.25     | 10       | 0.50       | 25     |
| Claude Opus 4.7  | 5     | 6.25     | 10       | 0.50       | 25     |
| Claude Opus 4.6  | 5     | 6.25     | 10       | 0.50       | 25     |
| Claude Opus 4.5  | 5     | 6.25     | 10       | 0.50       | 25     |
| Claude Opus 4.1  | 15    | 18.75    | 30       | 1.50       | 75     | (retired)
| Claude Sonnet 5.5| 2     | 2.50     | 4        | 0.10       | 10     |
| Claude Sonnet 5  | 2     | 2.50     | 4        | 0.20       | 10     |
| Claude Sonnet 4.6| 3     | 3.75     | 6        | 0.30       | 15     |
| Claude Sonnet 4.5| 3     | 3.75     | 6        | 0.30       | 15     |
| Claude Haiku 4.5 | 1     | 1.25     | 2        | 0.10       | 5      |
| Claude Haiku 3.5 | 0.80  | 1.00     | 1.60     | 0.08       | 4      | (retired)

Haiku 5.5 is priced by prompt length (<=100k: 0.10/0.125/0.20/0.01/0.50;
>100k: 0.50/0.625/1.00/0.05/2.50) — a shape this table cannot express, so it is
deliberately left to the fallback profile.

## Multipliers (quoted)

> * 5-minute cache write tokens are 1.25 times the base input tokens price
> * 1-hour cache write tokens are 2 times the base input tokens price
> * Cache read tokens are 0.1 times the base input tokens price (see the table
>   footnote for per-model exceptions)

Read exceptions: Fable 5.1 / Mythos 5.1 = 0.025x; Opus 5.5 / Sonnet 5.5 = 0.05x.
This is why cache read stays a per-model table value while the 1h write is
derived as `input_per_m * 2.0`.

## Context window (quoted)

> Claude 4.6 and later models (except Claude Haiku 5.5) and Claude Mythos
> Preview include the full 1M token context window at standard pricing.

=> Opus 4.6+/5/5.5, Sonnet 4.6+/5/5.5, Fable 5/5.1 = 1_000_000.
   Haiku 4.5 and everything 4.5-and-earlier = 200_000.

## What claudectl 0.68.0 shipped

Its "opus" profile is 15/18.75/1.875/75 — Claude **Opus 4.1** rates, retired,
and even its cache read (1.875) does not match Opus 4.1's actual 1.50. Its
"sonnet" is Sonnet 4.6 rates; its "haiku" is Haiku 3.5 rates. Because
`shorten_model` collapsed any unmatched opus id to the bare key "opus", Opus 5
was billed at 3x its real rate.
