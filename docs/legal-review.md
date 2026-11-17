# Legal review: benchmark publication, trademarks, dependency licences

This is the review half of the Phase 16 legal line. It records what the primary
sources actually say, at what level of verification each statement sits, and
which items still belong to counsel. It is a findings document, not legal advice,
and nothing in it should be quoted as a legal conclusion.

Closed by this document: **review each competitor's benchmark-publication terms
(kdb+ especially)**. Still open, and deliberately untouched here: the counsel
items (#384 **WEFTDB** trademark clearance, #386 the codec patent and licence
gate, #245 the benchmark policy, and the SAP-before-6.2 review).

## 1. How to read the verdicts

Each finding carries one of three verification levels. They are not
interchangeable, and the last one must never appear in public wording.

| Level | Meaning |
| --- | --- |
| **Read** | The primary text was read in full for this review; quotes below are verbatim, with the fetch date. |
| **Recorded** | Read earlier in this workstream and consolidated here from its notes. Re-read the live page before relying on it in a published claim. |
| **Inherited** | A claim repeated from `DECISIONS.md` that no session has checked against primary text. Useful as a risk flag; worthless as a reason. |

The distinction is not pedantry. The kdb+ line below was inherited for months,
and what the primary text says is narrower and more specific than the claim we
were repeating.

## 2. kdb+ / KX — the engine the roadmap singles out

Three instruments matter, and only two have a readable primary text today.

| Instrument | Who it binds | This review |
| --- | --- | --- |
| KDB On-demand Terms and Conditions v4 | customers operating under an Order Form | **Read**, 2026-11-15 |
| Kdb+ On Demand Software License Agreement for Non-Commercial Use (Personal Edition) | anyone who clicks "I Accept" or downloads the free edition | **Read**, 2026-11-15 |
| The KX Benchmarking Agreement — the consent instrument our standing decision points at | a third party KX permits to benchmark | **Not read**: the published URL returns HTTP 404 (2026-11-15) and no live copy was found |

### What the texts say

Commercial terms, clause 4.5, *Benchmark Test Results*, verbatim:

> You will not disclose any benchmark, test or performance information or any
> report which contains a competitive analysis regarding the Licensed Software or
> Services to any third party except as explicitly authorized in advance by us in
> writing.

The neighbouring commercial clauses matter to us for a second reason. Clause
1.2(b)(iii) forbids linking or distributing the Licensed Software with anything
"subject to the terms of a Copyleft License" — relevant to our own dependency
policy if we ever interoperate with kdb+ — and 1.2(b)(xi) forbids using the
Licensed Software "in order to build or offer a product or service … which
competes with … the Licensed Software or otherwise use or copy any features,
functions or graphics of the Licensed Software". We are not a KX customer, so
neither clause binds WeftDB as it stands. Counsel should nonetheless see them: if
we ever evaluate kdb+ under a trial or order form, 1.2(b)(xi) is the clause that
would make that evaluation itself a problem.

The free Personal Edition licence is the one an engineer would actually accept,
and it answers the benchmarking question directly. Clause 1.3, *Kdb+ On Demand
Software Performance*, verbatim:

> End User shall not distribute or otherwise make available to any third party
> any report regarding the performance of the Kdb+ On Demand Software, Kdb+ On
> Demand Software benchmarks or any information from such a report.

The rest of that licence makes the question academic for us:

- **1.1** grants a personal, non-transferable licence for *personal,
  non-commercial use only*, on at most two computers at 24 cores each, and
  expressly bars use "for production, testing, disaster recovery, training or
  commercial use", "for the benefit of a company", "for any revenue generating
  purpose", or "for research or development of a product or application which
  may … be substantially similar in its expression or functionality to the Kdb+
  On Demand Software". A vendor benchmark of a competing time-series database is
  squarely inside the excluded uses.
- **1.2** bars redistribution, decompilation, disassembly and reverse
  engineering, and bars running it on a third-party cloud provider's service.
- **1.4** times the installation out after six months.
- **1.5** phones home about once a minute with `.z.p`, `.z.a`, `.z.h`, `.z.o`,
  `.z.i`, `.z.u`, `.z.K`, `.z.k`, `\s`, `\p`, `.z.l`, the CPU affinity mask from
  `sched_getaffinity(2)` and CPU usage from `getrusage(2)`. Any kdb+ run on our
  hardware also publishes that hardware's identity and core layout to KX.
- **8** puts the agreement under New York law with exclusive New York venue.

### The consequence, stated carefully

The exclusion of kdb+ from every published comparison does not need the
inherited claim to justify it. It rests on the licence text: the only edition we
could obtain without paying is licensed for personal non-commercial use and its
§1.3 forbids making any performance report available to third parties, and the
commercial terms bar a customer from disclosing benchmark results without prior
written authorisation. We are not a customer, so §4.5 is not our obligation — but
there is no kdb+ we can run whose terms do not restrict either the run or the
report.

Two things this review does **not** settle, both for the owner and counsel:

1. **The consent path may not exist in a form we can read.** Our standing
   exclusion says "unless KX consents in writing". The instrument that grants
   that consent, `https://kx.com/legal/benchmarking-agreement_00xx_1-0-23-11-2022/`,
   returned HTTP 404 on 2026-11-15 and a search did not surface a live copy.
   Until someone obtains the current benchmarking-consent terms, treat "with KX's
   written consent" as an assumption, not an available route.
2. **Whether clause 1.2(b)(xi) reaches evaluation.** Reading a competitor's
   product to design a non-infringing codec is a different act from copying
   features, and this document does not adjudicate it.

### Public wording

Say: *kdb+ is not a comparison subject; the freely licensed edition is
non-commercial and forbids publishing performance reports (fetched 2026-11-15).*
Do not say: *kdb+'s licence forbids benchmarking by anyone* — that is broader
than the text, and it is exactly the kind of overreach the honesty page exists to
avoid. `docs/roadmap-notes.md` (the Phase 8 honesty line) already refuses
unverifiable "they lied" wording; the same discipline applies to licence claims.

Marks: **kdb+**, **KX**, **KDB-X** are KX marks. Use the names descriptively, in
the vendor's own spelling, with no logo and nothing implying endorsement. The
marks policy itself has **not** been read (**Inherited**).

## 3. Engines we race, and what their terms say about publishing

| Engine | Licence we rely on | Benchmark-publication terms | Level |
| --- | --- | --- | --- |
| ClickHouse | Apache-2.0 server, self-hosted | AUP §2 applies to *the Services* (Cloud and anything whose agreement references it), not to the OSS server; we race the server | Recorded |
| Timescale | Apache-2.0 distribution; the enterprise `tsl` tree is separately licensed | none found for the Apache-2.0 distribution | Recorded |
| QuestDB | Apache-2.0 | none found | Recorded |
| DuckDB | MIT | none in MIT; the project's **trademark** has its own policy | Recorded (licence) / Inherited (mark) |
| Apache IoTDB | Apache-2.0 | none; ASF project | Recorded |
| VictoriaMetrics | Apache-2.0 (its own terms page) | none found | Recorded |
| InfluxDB 3 Core | MIT / Apache-2.0 | no benchmark clause found in the Cloud terms surfaced 2026-11-15; the "InfluxData benchmark clause" in `DECISIONS.md` remains **unverified** | Inherited |
| Timescale's TSBS harness | MIT (`timescale/tsbs`) | not a subject; a workload harness we adapt | Read (commit-pinned) |
| Amazon Timestream | a service, not raced | "no benchmarking without prior written approval" | Inherited (`DECISIONS.md`, 2026-10-30) |
| SAP HANA, ClickHouse Enterprise | not raced | closed / enterprise terms, unread | Inherited |

Notes on the two rows that need the most care:

- **ClickHouse.** The Acceptable Use Policy section 2 (effective 2026-07-16, per
  the earlier note) does not ban publication. It conditions it: publish a
  benchmark of *the Services* and you must disclose to ClickHouse and include in
  the publication everything needed to replicate the run, and you accept that
  ClickHouse may benchmark and disclose results of your competitive products the
  same way. Results for beta or preview features are confidential. We benchmark
  the self-hosted Apache-2.0 server and do not use ClickHouse Cloud, so the
  clause is not engaged; and our published runs already carry the replication
  details — commit, versions, seeds, scale factors, hardware, methodology —
  because that is our own rule, not theirs. Before the first public ClickHouse
  number, someone should re-read the live AUP: it changed once already.
- **InfluxDB.** `weft-xbench/src/render.rs:39` entry 5 reads:
  *"Enterprise editions of QuestDB and InfluxDB 3 are excluded by their licences
  (see WILLINGNESS-TO-PAY.md)."* What was checked on 2026-11-15 is the free
  side: the InfluxData **Cloud** terms of use and the InfluxDB 3 **Core** MIT
  distribution, where no benchmark-publication clause was found. What was **not**
  read is the enterprise licence itself, which is what entry 5 actually cites —
  so the entry may well be right and this review does not contradict it. What it
  does contradict is any suggestion that we verified it: the citation behind
  entry 5 is **Inherited**, and it points at `WILLINGNESS-TO-PAY.md`, a document
  this repository does not have (`docs/`, `docs/archive/` and the root were
  searched on 2026-11-15). Reading the enterprise terms, or rewriting entry 5 to
  the reason we can actually show, is a small counsel task.

## 4. What an engineer must actually do

1. Race only engines that pass the `NOT_IN_SCOPE` gate. That gate is a code
   constant, not a doc: `weft-xbench/src/render.rs:39`
   `const NOT_IN_SCOPE: [&str; 7]`, whose entries carry the stated reason in the
   string itself. The renderer refuses to emit a row for an excluded engine, so
   the list is enforced; but the reason inside each string is not enforced
   against any citation, and entry 5 and 6 are exactly where an unverified
   licence claim can hide (see §3).
2. Every published comparison carries the replication set: commit, engine
   versions, seeds, scale factors, hardware, methodology link. That is our own
   honesty rule and it also happens to satisfy the ClickHouse condition if it ever
   applies.
3. Name engines in the vendor's own registered spelling; no logos; nothing that
   implies endorsement.
4. The `absent because <reason>` column in a report must match a cell in the
   benchmark object table, or say plainly "not benchmarked". Never print a legal
   reason that has no citation with a fetch date.
5. Never run a competitor benchmark on an evaluation, trial, personal or
   non-commercial licence without a counsel sign-off recorded in `DECISIONS.md`.
   This is the rule kdb+ makes concrete, and the phone-home clause (§1.5) is a
   second, independent reason not to run it on company hardware.
6. Do not quote a third-party benchmark number as if it were ours. The README
   announces the cross-engine comparison and names the engines it races
   (`README.md:179-180`); naming them is nominative use and is fine, but every
   number in that comparison is one we ran and can reproduce, and any number we
   did not run must carry its source and date.

## 5. Trademarks and naming

- Third-party marks (**kdb+**, KX, ClickHouse, Timescale, QuestDB, DuckDB,
  InfluxDB, Apache IoTDB, VictoriaMetrics, PostgreSQL, Amazon Timestream) are
  used descriptively only, and remain the property of their owners. This list is
  naming, not a clearance.
- **WEFTDB** is not yet cleared. #384 is open with counsel. Do not file, publish
  or assert anything about the mark off the back of this document.
- Do not confuse the mark question with US 11,308,093 (recorded in
  `DECISIONS.md` as granted to Amazon on 2025-01-14, claimed against the default
  FOR codec): that is a **patent**, and it belongs to #386.

## 6. Dependency licences — what the policy enforces

WeftDB is `MIT OR Apache-2.0`, with contributions under DCO sign-off or the CLA
(a settled decision).

Enforcement is automated and is the reason this section can be short:

- `deny.toml` `[licenses]` keeps a permissive allow-list (MIT, Apache-2.0,
  Apache-2.0 WITH LLVM-exception, BSD-2-Clause, BSD-3-Clause, BSL-1.0, CC0-1.0,
  ISC, Unicode-3.0, Unicode-DFS-2016, Unlicense, Zlib) at
  `confidence-threshold = 0.9`, and every departure from it is an `exceptions`
  entry with a written reason: `option-ext` MPL-2.0 (file-level copyleft, used
  unmodified, reached via `dirs`), `terminfo` WTFPL (reachable only on targets we
  do not ship), `webpki-root-certs` CDLA-Permissive-2.0 (wasm32 only, no shipped
  binary links it). The file also states that the allow-list and `about.toml`
  must move together, because a dependency accepted by one and not the other
  would ship in a release archive without its licence named in the bundle.
- The `cargo-deny` job (`.github/workflows/ci.yml:391`) runs
  `cargo deny check advisories bans licenses sources` under a pinned
  `cargo-deny` (`:394-399`). A separate job, named `third-party licenses`
  (`.github/workflows/ci.yml:401`), runs `cargo about generate` for four
  reporters — markdown, html, spdx, cyclonedx (`:418-421`) — driven by
  `scripts/licenses/generate.sh`, and **fails if the committed bundle does not
  match the lock file**; `THIRD-PARTY-NOTICE.txt` is generated from the
  `license-file` of every crate in the graph and shipped in release artefacts
  (`:425-429`). Both jobs were green on main on 2026-11-15.
  **Update 2026-11-17:** that green state ended for the advisories check when RustSec
  marked `chrono` unmaintained (RUSTSEC-2026-0543, "no safe upgrade"): the job now fails
  on every branch. It is accepted in `deny.toml` with a reason naming 2027-01-31 as the
  revisit date — a machine-enforced `expiry` needs cargo-deny 0.21.0 and CI pins 0.20.2
  through the action, so # bumps that pin — and
  `unused-ignored-advisory = "deny"` keeps a dead exception from sitting around; the
  migration off `chrono` is #791 (filed as #790). A licensing document is the wrong place to
  hide a dependency risk, so it is recorded here as well as in `DECISIONS.md`. The job sits
  at `.github/workflows/ci.yml:410` on current main; the numbers above are as of the review
  date.
- Vendored source travels in `patches/` and is wired through `patch.crates-io`
  (`Cargo.toml:166-167`). The upstream licence travels with the patch and shows
  up in the bundle.
- **Checked while doing this review, because a note in this workstream said
  otherwise:** `splimes` is a **crates.io** dependency, `version = "1.3"`
  (`Cargo.toml:109`, `default-features = false, features = ["serde"]`); the two
  feature-gated uses in `weftdb/Cargo.toml:55` and `:74` add `tokio` and `gpu`.
  It is **not** recorded there as `MIT OR Apache-2.0`, which is what that note
  asserted, and there is **no** git pin to audit — the note described a state
  that does not exist on main. Its acceptance is decided by `deny.toml` and the
  generated bundle, which admit it; our own `weft-reduce` crate imports it
  (`weft-reduce/src/lib.rs:40`). If anyone intends to keep developing `splimes`
  in the open, its licence and maintenance status are the things to confirm, and
  that belongs with #386 rather than here.

**Nothing else was found while reviewing, and two claims made earlier in this
workstream have to be retracted here so they do not become repo folklore.** A
note claimed the TUI embeds fonts under `MIT OR OFL-1.1` and that SIL OFL is
missing from `deny.toml`. That is not true on main: `weft-tui/assets/` does not
exist, no source file mentions `OFL`, and no `OFL` string appears in
`THIRD-PARTY-LICENSES.md`. Do not open a follow-up to add `OFL-1.1` to the
allow-list. The same note claimed a `license-bundle` job fails on main with
lock-file drift: the job is named `third-party licenses`, and it was **green** on
the latest main run checked on 2026-11-15, together with `cargo-deny`. If that
drift comes back it is a CI problem, not a licensing one, and it does not belong
in a legal document.

## 7. Codec licences, and the pin nobody has verified

- The bitsliced FOR codec is **feature-gated off by default**, and the gate is
  real. The feature is declared without a default (`weftdb/Cargo.toml:20-24`;
  `default = ["std"]` at `:18-19` does not include it), the modules are
  `#![cfg(any(test, feature = "bitsliced-codec"))]`
  (`weftdb/src/types/bitsliced_for.rs:1`,
  `weftdb/src/types/segment_store/bitsliced_for_codec.rs:1`), and the encode
  path falls back to the default FOR codec when the feature is absent
  (`weftdb/src/types/segment_store.rs:233,241,6306`); a
  `#[cfg(feature = "bitsliced-codec")]` block covers the codec's own use of the
  type at `:5661-6293`. A test pins that the transposition tuning variable is
  ignored without the feature (`weftdb/tests/bitsliced_codec_feature.rs:4-7`).
  So a default build puts no bitslicing code in the encoder's path and links no
  third-party codec. What does **not** exist is a licence acknowledgement or any
  compile-time refusal to enable the feature: the enforcement is "off unless you
  ask for it", not "refused until cleared", and that gap is exactly the Phase 16
  "advisory codecs: not yet reviewed, and ungated" line.
- `DECISIONS.md` (2026-11-08, "the codec pin is not verified") records that a
  crate pin once pointed at an upstream whose licence had never been checked, and
  that the current pin's provenance is still unverified. This review does not
  retire that record. The Phase 16 line "**the 2026-10-08 routine's advisory
  codecs: not yet reviewed, and ungated**" stays open, and #386 (Amazon
  US 11,308,093 against the default FOR codec, the Tiger Data family, the SAP
  family before the 6.2 design) is unchanged by anything in this file.

## 8. Open items after this review

| Item | Owner | Where |
| --- | --- | --- |
| **WEFTDB** trademark clearance | counsel | #384 |
| Codec patents and licences (US 11,308,093, Tiger Data family, SAP before 6.2, the ungated advisory codecs) | owner + counsel | #386, Phase 16 codec gate |
| The benchmark policy itself | owner + counsel | #245 |
| Obtain the current KX benchmarking-consent instrument (the published URL 404s) | owner | this document §2 |
| Re-read the live ClickHouse AUP before the first public ClickHouse number — the "effective 2026-07-16" date in §3 came from an ephemeral note and was not re-verified here | whoever runs the next published run | §3 |
| Confirm or retire the licence reason inside `NOT_IN_SCOPE` entry 5, and find or replace the `WILLINGNESS-TO-PAY.md` it cites, which this repository does not contain | counsel, or a careful read of the enterprise terms | §3 |
| Confirm `splimes`' licence and maintenance status: `Cargo.toml:109` states no SPDX expression, so acceptance is decided today by `deny.toml` and the generated bundle | repo owner | §6 |

Fetch log for the **Read** rows: KDB On-demand Terms and Conditions v4
(`https://kx.com/legal/terms-and-conditions-v4/`) and the Kdb+ On Demand
Non-Commercial licence (`https://kx.com/legal/kdb-free-personal-edition-license-agreement/`),
both fetched 2026-11-15. ClickHouse's Acceptable Use Policy and InfluxData's
Cloud terms of use were surfaced earlier in the same workstream and **neither was
re-read for this document**; their clause summaries are **Recorded**, not
**Read**, and the live page governs. The dead KX benchmarking-agreement URL is
recorded in §2 as a dead end. Inherited rows keep the citation they already carry
in `DECISIONS.md`, `docs/roadmap-notes.md` (the Phase 8 honesty line) or
`weft-xbench/src/render.rs:39`; none of them has been verified here. Every
in-repo citation in this file was checked against the tree on 2026-11-15, and
the two claims that failed that check are retracted in §6 rather than kept as
footnotes.
