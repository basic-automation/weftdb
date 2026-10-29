# Willingness-to-pay hypotheses

Phase 0 deliverable: *Document willingness-to-pay hypotheses* ([ROADMAP](ROADMAP.md), Phase 0).
Drafted 2026-10-29. Companion: [DESIGN-PARTNERS.md](DESIGN-PARTNERS.md).

**Every hypothesis in this document is untested.** None has been put to a buyer. The price bands
are starting guesses to be proven wrong, not quotes, forecasts or targets. Nothing here is a
claim about WeftDB's market and none of it may be repeated in the README as one.

## What we are pricing, and what we are not

WeftDB is dual-licensed MIT/Apache-2.0, interpolation included. So no hypothesis below is
"they pay for interpolation". Each is about what a buyer pays for *around* the open engine:
support and an SLA, hardened deployment (TLS, auth, RBAC and packaging are 1.0 criteria and not
shipped yet), managed hosting (there is none), benchmark work on their data, or a paid tier whose
boundary is not decided (see "Needs human").

The governing question is the one the original commercial analysis left open: **is interpolation
performance a budget-owning pain, or an engineering annoyance?** If it is an annoyance, a
buyer writes ten lines of pandas and pays nobody. Every test below is designed to answer that
question for one segment before anything expensive is built.

## Constraints from what has been measured

Hypotheses may only lean on what WeftDB has shown, or on what a test would show.

- **A spline fill does not beat linear on real prices.** Real BTC closes, hourly buckets: cubic MAE
  $55.34 against linear $49.27; minute buckets $8.37 against $6.79, at ~14% more time. The spline
  wins only on a generated smooth signal (RMSE 0.115 against linear's 0.865). No hypothesis for
  price data assumes better fill accuracy, and no hypothesis anywhere assumes spline accuracy on
  real data until a real smooth-signal corpus has been run (open ROADMAP item).
- **The GPU is not the fastest path for stored-data interpolation.** ~1M reconstructed points took
  14.1–16.3 ms on rayon and 17.0–29.8 ms on the GPU. No hypothesis sells GPU speed.
- **Exact decimals are roughly at parity with the competition, not ahead.** Hourly `avg` over 1M
  real BTC closes (README, 2026-10-11): exact on stored mantissas ~2.1× an `f64` loop, ~10× on the
  `BigDecimal` path. QuestDB ships a native `DECIMAL` at a documented ~2× `DOUBLE`. Precision is
  not a category of one.
- **What is distinctive and shipped:** every output point marked raw / interpolated /
  extrapolated; resampling onto a regular grid server-side (`POST /api/v1/interpolate`); exact
  decimal reductions including three time-weighted averages (`twa`, `twa_linear`,
  `twa_bucket_end`); declared physical encodings that refuse silent rounding at ingest.

The point-marking is the one property no incumbent named below provides as a first-class output.
The hypotheses lean on it, on doing the resample in the database instead of in application code,
and on linear/LOCF fill — not on spline accuracy.

## Value metrics considered

| Metric | For | Against |
| --- | --- | --- |
| $ per billion interpolated output points at a p95 target | The north star; ties price to the differentiator | Buyers do not count output points; needs a calculator before anyone can budget it |
| Per node / per core, annual | How historian and on-prem TSDB buyers already budget | Decoupled from the differentiator |
| Per site / per plant, annual | Matches how OT budgets are held | Sites differ by orders of magnitude in volume |
| Support subscription, flat tiers | Simplest for an open-source engine | Says nothing about value; easy to decline |
| Fixed-fee engagement (benchmark report) | Sells before 1.0; no product risk for the buyer | Services, does not scale |

Working assumption (untested): self-hosted buyers budget per node or per site, and
$/billion-points is the *internal* metric used to check that a per-node price is under the cost
the buyer pays today. A hosted offer, if one ever exists, would price on output points directly.

## Hypotheses by segment

Each segment: who pays · job and pain · what they pay for today · value metric · hypothesis and
band · confirm / kill signal · cheapest test.

### H1 — Industrial and process data teams (OT analytics) — UNTESTED

- **Who pays:** the engineering or data lead for operational data at a mid-size manufacturer or
  process plant; budget sits with OT/IT infrastructure or a digital-operations programme.
- **Job and pain:** historians store change-of-value or deadband-compressed tags, so a tag that has
  not moved has no recent sample. Analytics and ML need those tags on one regular grid, and the
  team needs to know which grid values were measured and which were inferred. Today that
  alignment is re-done in Python per project, and the inferred values are not labelled.
- **Pay for today:** historian licences and interpolated-retrieval modes (AVEVA PI System, Canary,
  Ignition historian modules), cloud IoT services (AWS IoT SiteWise, Azure Data Explorer), general
  TSDBs (InfluxDB, TimescaleDB), plus engineer time.
- **Value metric:** per site, annual; checked against $/billion output points.
- **Hypothesis:** at least one in three such teams spends recurring, named effort on
  grid-alignment and gap labelling, and would pay **$10k–$40k per site per year** for a supported
  WeftDB deployment beside (not replacing) their historian.
- **Confirms:** interviewees name a current cost line (engineer-weeks per quarter, a cloud bill, a
  historian module) for this work; two or more ask for a pilot on their own exported tags.
- **Kills:** "the historian already does that" and they do not care which points are inferred; or
  the pain exists but no budget owner can be named.
- **Cheapest test:** 8–10 discovery calls with OT data leads, using a fixed question script (below),
  then one `weft-bench` run on an exported tag set from whoever offers one.

### H2 — Grid, metering and energy-forecasting teams — UNTESTED

- **Who pays:** a distribution operator's or energy retailer's analytics or forecasting team.
- **Job and pain:** meter, SCADA and weather inputs arrive at different rates with gaps; forecasting
  pipelines reindex to a fixed grid (15 min is common) and either fail on gaps or fill them.
  Metering has an existing concept that matches WeftDB's marking: validation, estimation and
  editing (VEE), where an estimated read must be distinguishable from a measured one.
- **Pay for today:** meter data management systems with VEE modules, forecasting platforms,
  open-source forecasting stacks (LF Energy OpenSTEF) on a general TSDB or data lake, staff time.
- **Value metric:** per node or per platform, annual.
- **Hypothesis:** a forecasting or analytics team (not the billing system of record) would pay
  **$20k–$75k per year** for a supported engine that resamples multi-rate inputs in the store and
  carries the measured/estimated distinction through to the model input.
- **Confirms:** the team reports gap handling as a cause of forecast failures or manual rework, and
  says provenance of filled values matters to an internal or regulatory reviewer.
- **Kills:** gaps are handled upstream by the MDMS and the team only ever sees clean grids; or
  procurement requires certifications WeftDB cannot have before 1.0.
- **Cheapest test:** read OpenSTEF's data-integration path and issues for gap-handling pain, then
  5 calls with forecasting teams; one replay of a public load + weather dataset through
  `weft-bench --gap-fill` with linear fill scored against held-out real values.

### H3 — Research facilities and environmental monitoring networks — UNTESTED

- **Who pays:** controls or data group at an accelerator, observatory, hydrology or air-quality
  network. Often grant- or agency-funded.
- **Job and pain:** archivers record on change (EPICS monitor mode); station networks have dropouts
  and publish estimated-value qualifiers; analysts resample to common grids by hand.
- **Pay for today:** mostly nothing in licence terms — EPICS Archiver Appliance, HDF5/NetCDF,
  InfluxDB OSS, in-house code. Staff time is the cost.
- **Value metric:** support subscription, or in-kind (data, co-authorship, published results).
- **Hypothesis:** this segment pays **under $10k per year or in kind**. It is a design-partner
  source — real smooth physical signals, the corpus the spline claim needs — not a revenue
  segment.
- **Confirms (as partner):** a group shares a real sensor corpus and runs `weft-bench` on it.
- **Kills (as payer):** expected; if a group does offer budget, revisit.
- **Cheapest test:** ask for a public or shareable dataset; no pricing conversation.

### H4 — ML teams building features from irregular series — UNTESTED

- **Who pays:** an ML platform or data engineering lead whose training pipelines resample irregular
  series into fixed-step features.
- **Job and pain:** resample, gap-fill and point-in-time alignment run in Spark, pandas or Polars
  on every training run; filled values leak into labels or features unmarked.
- **Pay for today:** Databricks/Spark compute, warehouse compute, feature stores (Feast open
  source, Tecton), engineer time.
- **Value metric:** $ per billion interpolated output points, compared with their current compute.
- **Hypothesis:** where resampling is a measurable share of pipeline compute, a team would adopt
  WeftDB if the measured $/billion output points is **at least 3× below** their current cost, and
  would pay for support at **$5k–$25k per year**. No absolute $/billion price is proposed until
  `weft-bench` publishes a cost figure.
- **Confirms:** a team can state resampling compute cost and it is non-trivial.
- **Kills:** resampling is a rounding error next to training compute; or they need the work inside
  their existing Spark job and will not add a service.
- **Cheapest test:** 5 calls; ask for their resampling step's runtime and cluster size. Needs the
  `weft-bench` cost-per-billion-points figure first, which is a Phase 1 output.

### H5 — Market-data and small quant/crypto desks — UNTESTED, weakest

- **Who pays:** a small fund, trading desk or market-data vendor without a kdb+ licence.
- **Job and pain:** exact decimal aggregates and time-weighted averages; knowing which bars are
  carried forward. **Not** fill accuracy: on prices linear beats spline, and LOCF is often the
  correct rule anyway.
- **Pay for today:** kdb+, OneTick, QuestDB (now with `DECIMAL` and `twap`), TimescaleDB, Parquet
  plus DuckDB.
- **Value metric:** per node, annual.
- **Hypothesis:** a desk would pay **$5k–$25k per year** for exact-decimal `twa` and provenance
  marking that QuestDB and TimescaleDB return only as `double`.
- **Confirms:** the desk has been bitten by float drift in a reported number, and can show it.
- **Kills:** QuestDB's decimal and `twap` are good enough; or they will not consider anything
  without a track record.
- **Cheapest test:** publish a `weft-bench` TWA comparison (exact vs `double` deviation) once the
  external adapters exist, and count inbound interest. Not before.

### H6 — Benchmark engagement before 1.0 — UNTESTED

- **Who pays:** any team above that is choosing or replacing a TSDB.
- **Job and pain:** they need a fair comparison of engines on their own workload and cannot spend
  the weeks to build one.
- **Pay for today:** consultants, vendor proof-of-concepts (free, and biased), internal time.
- **Value metric:** fixed fee per report.
- **Hypothesis:** a team would pay **$5k–$20k** for a reproducible `weft-bench` report on their
  data across WeftDB and the engines they already use, including results where WeftDB loses.
- **Confirms:** one paid engagement in the first ten offers.
- **Kills:** no takers when the report is free either.
- **Cheapest test:** offer it to the design partners; free first, priced second.

## Which hypothesis to test first

**H1, run together with H3's data request.**

1. It does not depend on anything unshipped or unproven. Linear and LOCF fill, server-side
   resampling and the raw/interpolated/extrapolated marking all exist today. H4 needs a published
   cost figure; H5 needs external adapters; H2 is H1 with slower procurement.
2. Its kill signal is cheap and early: ten calls answer "budget-owning pain or annoyance" for the
   segment where the pain is most plausible.
3. It produces the missing benchmark input. A historian export of physical tags (temperature,
   power, vibration) is the real smooth-signal corpus the ROADMAP needs before any spline-fill
   claim. A partner who supplies one advances Phase 0 and Phase 1 at once.
4. The marking maps onto a need the buyer already has a word for (measured vs estimated), so the
   differentiator does not have to be explained from scratch.

If H1 is killed, test H2 next (same mechanism, different buyer), then H4 once the cost figure
exists. Do not price anything publicly before one of H1, H2 or H4 has a confirm signal.

## Discovery question script (H1/H2)

Ask about the past, not about WeftDB. Do not pitch before question 6.

1. Walk me through the last time you had to put several tags or meters on one time grid.
2. What did it take — who, how long, which tools?
3. How often does that happen? Is it one person's recurring job?
4. When a value was missing, what did you put there? Did anyone downstream know?
5. Has an inferred value ever ended up in a report, model or alarm and caused a problem?
6. If the store returned the grid with each point labelled measured or inferred, what would that
   replace? Who would have to approve buying it?
7. What do you spend today on the tools involved (licence, cloud, people)?

Record answers per interview in a dated note. A confirm needs evidence from questions 2, 3 and 7,
not enthusiasm at question 6.

## Recommended boundary and opening prices (2026-10-29, not yet decided)

Researched against what comparable products gate and charge. These are recommendations for the
operator to accept or change. Nothing is for sale yet (see Decided, below).

### What comparable products gate

| Product | Free | Paid | Price published? |
| --- | --- | --- | --- |
| QuestDB | Apache-2.0 single node; basic auth, **no TLS** | TLS, RBAC down to columns, replication, multi-primary, HA ([comparison](https://questdb.com/enterprise)) | No: AWS Marketplace private offers only |
| InfluxDB 3 | Core: MIT/Apache single node, no compaction; Enterprise free "at home" on 2 cores | Enterprise: HA, read replicas, compaction, security; cores sold in 8/16/32/64/128 batches ([licence docs](https://docs.influxdata.com/influxdb3/enterprise/admin/license)) | No: contact sales |
| TimescaleDB | Apache-2 edition; most features under the source-available TSL, free unless resold as a hosted DB ([editions](https://www.tigerdata.com/docs/about/latest/timescaledb-editions)) | Tiger Cloud (hosted), tens of dollars a month to start | Cloud yes, self-hosted n/a |
| Canary historian | — | 3,500 tags: about $3,000/yr subscription or $8,730 perpetual ([pricing](https://info.canarylabs.com/pricing)) | Yes |
| Ignition | — | Per server, unlimited tags: platform about $1,200 plus modules, about $12–15k per bundle (secondhand, [G2](https://www.g2.com/products/inductive-automation-ignition/pricing)) | Partly |
| AVEVA PI | — | Mid-size deployments reported at about $108–162k/yr; one city's award came to $108,570/yr (secondhand) | No |

The pattern: database vendors keep single-node engines open and charge for what a team or a
company needs and an individual does not (HA, replication, fine-grained access, audit), with
prices behind a sales call. Historians charge per tag or per server, from a few thousand dollars
a year (Canary) to six figures (PI). Ignition's "unlimited tags per server" is its main selling
point against per-tag pricing.

### Recommended boundary

**Free and MIT/Apache, permanently:** everything one engineer needs on one node. That covers the
engine, every interpolation and fill method, the raw/interpolated/extrapolated marking, ingest,
storage, the compute and storage endpoints, the GPU backend, `weft-bench`, **and TLS plus basic
token auth**.

- The differentiator has to be free, or there is nothing to benchmark in public, and no H1 or H2
  team will put it beside a historian.
- TLS and auth are already in the 1.0 release criteria. Gating them, as QuestDB does, would
  contradict the roadmap and make every pilot a security exception.
- Existing code stays under MIT/Apache. Relicensing what is already open is the Elastic/HashiCorp
  move, and it would cost the trust a benchmark-first project depends on.

**Paid, when there is an entity to sell it (buyer-based: what a team or company needs):**

1. **HA and replication** (read replicas, failover) and scheduled, verified backup and restore.
2. **Enterprise access:** SSO (OIDC/SAML/LDAP), fine-grained RBAC and audit logs.
3. **Historian connectors** that map a historian's quality flags onto WeftDB's provenance
   (OPC UA, PI and Canary export import). This is H1-specific, and it is where the marking turns
   into money.
4. **Support with an SLA**, plus **benchmark engagements** (H6).

Mechanically: paid features live in separate crates, the way Timescale keeps its `tsl/` tree
apart. The CLA, which names Justin Icenhour as recipient, is what allows contributed code to sit
beside proprietary modules later. That is for a lawyer to confirm when the entity is formed.

**Not recommended:** per-tag or per-aspect pricing (it invites the PI comparison and loses to
Ignition's unlimited model); hosted or $/point pricing before a hosted service exists; gating any
interpolation method.

### Recommended opening prices (for discovery calls, not published)

| Offer | Opening price | Anchor |
| --- | --- | --- |
| Design-partner pilot | Free during the pilot; 50% off the first two years if they convert | Pays for the data and the reference |
| Support only, one node | $3,000/yr | Canary's 3,500-tag subscription is about $3,000/yr |
| Production, per node (HA, SSO/RBAC/audit, connectors, business-hours support) | $12,000/yr | Inside H1's $10–40k band; about one Ignition bundle a year; far under PI |
| Site / plant (unlimited nodes and tags, 24×7) | $30,000/yr | The top of H1, the bottom of H2, a fraction of a mid-size PI deployment |
| Benchmark report on the buyer's data | $7,500 fixed | Inside H6's $5–20k; one engineer-week of a consultant |

Quote the per-node price first and ask what they pay today (question 7). A team that names a
historian bill will anchor against it, and that answer is the evidence H1 needs. Revisit after
five calls; move a price only on evidence from the calls, not on comparables.

## Decided (2026-10-29)

- **Who makes contact:** the operator, as **Basic Automation**. Basic Automation is not yet a
  company and will not be incorporated for now. Until it is, calls are discovery only. Use the
  question script, ask what they pay today, and quote prices as "what would you say if…". No
  quotes, invoices, contracts or paid pilots until there is an entity to sign them.
- **Pilots without TLS or auth:** acceptable now as an isolated technical test (for example,
  `weft-bench` on a partner's exported data, or a server on their network with no outside
  exposure), and **not** acceptable for 1.0, where TLS and auth are release criteria.

## Needs human

- **Accept, change or reject the recommended boundary and prices above.**
- **Whether to offer hosting at all.** H4's natural metric is per output point, which implies a
  hosted service the project does not have and the README says does not exist.
- **Whether design partners get founding pricing,** and whether 50% for two years is the number.
- **Benchmark-publication terms.** H5 and H6 publish comparisons against commercial engines; each
  vendor's terms (kdb+ in particular) need checking before anything is published.
- **Where this document lives.** `docs/` was removed from `main` in #99 and the prose moved to
  basicautomation.io, which this repository cannot write to. It sits at the root beside
  `ROADMAP.md` and `DECISIONS.md` until it has a page on the site (DECISIONS.md, 2026-10-29).
