# Benchmark policy: what we may race, and what we must say when we publish

Status: **draft for counsel review** (issue #245, opened by #383's legal
review). The rules below are in force for engineers now; counsel may tighten
them, and nothing here clears anything counsel has not looked at.

This is the operational half. The evidence — what each vendor's terms actually
say, with the level of verification behind each claim — lives in
[Legal review](legal-review.md). Where this file states an obligation, the
citation is in that document, not repeated here.

## 1. The gate is code

`weft-xbench/src/render.rs:39` holds `const NOT_IN_SCOPE: [&str; 7]`. The
renderer refuses to emit a row for an engine in that list, so exclusion is
enforced mechanically. What is *not* enforced is the reason written inside each
entry string.

- Changing an entry — adding or removing an engine — is a legal change, not a
  benchmark change. The PR must carry a citation for the new reason with the
  date the source was read, and update the matching row of the review table in
  `docs/legal-review.md` §3. A PR that edits the constant and not the table is
  wrong by construction.
- An entry whose reason cannot be cited is a defect, not a policy. Entry 5
  (enterprise editions of QuestDB and InfluxDB 3) currently cites a document
  this repository does not have; that is tracked as an open counsel item, and it
  stays excluded until either the enterprise terms are read or the entry is
  rewritten to a reason we can show.

## 2. What we race, and on what terms

Self-hosted open-source servers only. We do not benchmark a vendor's hosted
service, and we do not use a hosted service to do benchmarking: a terms-of-use
clause barring use of the service for developing competing products
(ClickHouse's AUP §1.5 is the example) is a reason not to sign up, independent
of anything about publication.

| Engine | We race | Publication obligation |
| --- | --- | --- |
| ClickHouse | the Apache-2.0 server, self-hosted | AUP §2 is not engaged while we stay off the Services; if we ever race the Cloud, publication requires disclosure to ClickHouse plus full replication details, and accepts reciprocity |
| TimescaleDB | the Apache-2.0 distribution | none found; keep the enterprise `tsl` tree out of scope |
| QuestDB | Apache-2.0 | none found |
| DuckDB | MIT | no licence condition; the project's trademark has its own policy |
| Apache IoTDB | Apache-2.0 | none (ASF project) |
| VictoriaMetrics | Apache-2.0 | none found |
| InfluxDB 3 Core | MIT/Apache-2.0 distribution | no benchmark clause found in the free-side terms; the enterprise licence that entry 5 relies on is unread |
| kdb+ / KX | **not raced** | see §4 — consent is required and we do not hold it |
| Amazon Timestream, SAP HANA, ClickHouse Enterprise | not raced | service or closed terms; Timestream requires prior written approval even to run one |

## 3. Every published number is ours, and reproducible

A comparison may be published only if, for every engine row, we can point at the
run that produced it. Required in the published artifact or its methodology link:

- our commit, and each engine's exact version;
- the workload and its seeds, scale factors, and cardinalities;
- the hardware and storage configuration the run used;
- the harness and flags, at the commit the run used;
- for any absent engine, the reason, which must be either a `NOT_IN_SCOPE`
  entry or the words "not benchmarked" — never an invented legal reason.

A third-party number may be quoted only with its source and date, and never
formatted so that it reads as one of ours. The README names the engines we race;
naming is fine, and so is describing what we did not race, provided the reason
is one we can show.

## 4. kdb+ and AVEVA PI stay out until their vendors answer

The free personal/non-commercial kdb+ licence bars use for anything that furthers
a commercial enterprise — including testing that decides which engine to build
on, or evaluating KX products — and separately bars publishing benchmark or
performance reports of licensed software. The commercial "On Demand" terms bind
the customer's *users*, so a hosted instance raced by our own employees would
need KX's agreement as well.

KX publishes a Benchmarking Agreement as the consent route, but the URL in the
terms text returns 404 as of 2026-11-15, so nobody in this repository knows how
to obtain that consent. Until then:

- No kdb+ on company hardware or accounts, in any edition, personal included.
  A second, independent reason not to install it there: its EULA compels
  transmission of usage and hardware details to KX.
- Do not repeat the claim that kdb+ is "under a free licence" without the two
  sentences above attached to it. That claim circulates in prose —
  `docs/roadmap-notes.md:45` says only "kdb+ is excluded by its licence" — and
  the gate string itself, `weft-xbench/src/render.rs:39` entry 6, reads:
  *"kdb+ and AVEVA PI: their licences bar benchmarking or publishing results
  without written consent."*
- **AVEVA PI** shares that entry and has had no terms review at all: the
  sentence in the code asserts a restriction nobody in this repository has read
  in this cycle. It is harmless while the engine stays excluded, and it becomes
  a liability the moment anyone proposes racing it. Reading AVEVA/OSisoft PI's
  licence is the same kind of counsel task as the InfluxDB enterprise terms.
- Getting the consent path (or a written no) is a **Needs human** item: it needs
  a conversation with KX, not a code change. Track it under the counsel items in
  `DECISIONS.md`.

## 5. Licences we run under, and evaluation traps

Never run a competitor under an evaluation, trial, demo, personal or
non-commercial licence on company equipment without a counsel sign-off recorded
in `DECISIONS.md`. Several of the exclusion entries in the code gate are really
about this: a licence that forbids commercial evaluation is not a licence we can
run under, and "we only ran it once to check" is not a defence.

## 6. Naming

Use the vendor's own spelling of its mark (kdb+, ClickHouse, TimescaleDB,
QuestDB, DuckDB, InfluxDB, Apache IoTDB, VictoriaMetrics). No logos, no
trade-dress, nothing that reads as endorsement or affiliation. **WEFTDB** is
used as the project's name and is not yet cleared; #384 is open with counsel,
and nothing in this file is a clearance or a filing instruction.

## 7. Re-read cadence

Terms change: ClickHouse's AUP took effect 2026-07-16, and the KX benchmarking
route has moved. The review table carries a fetch date per row; if a row's date
is more than a year old and we are about to publish a number for that engine,
re-read it first and update the row in the same PR as the number.

## 8. What this policy does not decide

Counsel still owns: the codec patent questions (US 11,308,093 and the FOR,
Tiger Data and SAP families — #386), **WEFTDB** trademark clearance (#384), the
package licence expression, the InfluxDB and QuestDB enterprise terms behind
`NOT_IN_SCOPE` entry 5, the licence behind `NOT_IN_SCOPE` entry 6's AVEVA PI
half, and the kdb+ consent route. This file gives them a draft to react to; it
is not a decision from them.
