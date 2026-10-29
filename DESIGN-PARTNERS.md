# Design-partner programme

Phase 0 deliverable: *Identify 3–5 design partners with interpolation-heavy workloads*
([ROADMAP](ROADMAP.md), Phase 0). Drafted 2026-10-29. Companion:
[WILLINGNESS-TO-PAY.md](WILLINGNESS-TO-PAY.md).

**Status: nobody has been contacted.** The shortlist below is a set of archetypes with public
organisations or open-source projects that *plausibly* have the workload, chosen from public
documentation. It records no contacts, no names, and no expression of interest from anyone.
Choosing the 3–5 to approach, and approaching them, is the operator's call.

## What a design partner is for

Three things, in order:

1. **A real workload to measure.** `weft-bench` scores fills against real values removed from a
   real series. The only real corpus so far is a price series, where linear fill beats spline. A
   partner with physical sensor data supplies the smooth-signal corpus the ROADMAP needs before
   any "spline fill is more accurate" claim — or shows the claim is false.
2. **An answer to the governing question** for one segment: is interpolation a budget-owning pain
   or an engineering annoyance? (See WILLINGNESS-TO-PAY.md.)
3. **A reference for the 1.0 acceptance bar:** a partner can deploy WeftDB, ingest, run
   `weft-bench`, and read the result.

A partner is not necessarily a buyer. Several of the strongest workload fits below are
publicly funded and will not pay licence fees; the programme should include at least one partner
from a segment that could pay (H1 or H2 in WILLINGNESS-TO-PAY.md).

## Ideal-partner profile

The workload, not the organisation, qualifies a partner. Interpolation must be on the critical
path, not a nicety. Traits, strongest first:

- **Irregular sampling by design.** Change-of-value or deadband archiving, event-driven devices,
  report-by-exception telemetry: the store has no sample where nothing moved.
- **Sensor dropouts that must be filled and labelled.** Outages, missed transmissions, maintenance
  windows; downstream someone needs to know which values were measured.
- **Multi-rate alignment.** Several sources at different rates (1 Hz, 15 min, hourly) joined on one
  grid for analysis or a model.
- **Resampling for ML features.** Training pipelines that reindex irregular series to fixed steps
  on every run.
- **Regulatory or audit reconstruction.** A reconstructed value must be distinguishable from a
  measured one in a report (metering estimates, environmental compliance, data qualifiers).
- **Smooth physical signals.** Temperature, pressure, flow, power, vibration — where a spline fill
  *might* help, and where the honest test of that is still missing.
- **Large historical ranges** read repeatedly, so doing the work in the store rather than in
  application code changes latency or cost.
- **Measurable targets:** a latency or cost number the partner already tracks.

## Disqualifiers

- Regular, gap-free data. WeftDB's advantage does not apply (README, "A poor fit").
- Generic dashboards or monitoring only; no reconstruction downstream.
- Needs TLS, authentication, RBAC or managed hosting now. None is shipped; a pilot must run on an
  isolated network.
- Needs per-measurement tags or high-cardinality dimensional slicing (not implemented).
- Wide-table OLAP as the main workload (ClickHouse/DuckDB territory, not chased).
- Cannot share any data or results, even anonymised or synthetic-but-representative.
- Price-only data with the expectation that spline fill improves it. Measured: it does not.
- Regulated personal data (clinical, wearables) unless the partner already works on a
  de-identified, credentialed research dataset and the compliance scope is theirs.

## What a partner gives

- One representative dataset, or a `weft-bench` run on their own hardware with the JSON/Parquet
  report returned. Anonymised or rescaled values are fine if the timestamps and gaps are real.
- The fill rule they use today and the error they can tolerate.
- A 30-minute call roughly monthly for three months, and written answers to the discovery script.
- Permission to publish anonymised results, including results where WeftDB loses — or an explicit
  "no", recorded.

## What a partner gets

- A reproducible `weft-bench` report on their workload: WeftDB against their current method
  (in-DB fill, SQL baseline or client-side code), with error against held-out real values, latency
  and storage, and the hardware recorded.
- Direct engineering attention on the paths their workload exercises.
- Input into which Phase 1–2 items come next (`?fill=` on downsample endpoints, arbitrary bucket
  widths and external-engine adapters are all open).
- Commercial terms, if any, are not set — see "Needs human".

## Qualification checklist

Score each yes. Approach candidates with 6 or more, and require items 1, 2 and 8.

1. [ ] Data is irregular, gappy or multi-rate (not merely late).
2. [ ] Someone fills, resamples or aligns it today, in code or a tool we can name.
3. [ ] Filled values reach a model, report, alarm or invoice.
4. [ ] It matters downstream which values are measured and which are inferred.
5. [ ] At least one source is a smooth physical signal (useful for the spline test).
6. [ ] A latency, cost or error target exists and they can state it.
7. [ ] Historical range is large enough to matter (months to years, or 10M+ points per series).
8. [ ] They can share data or run `weft-bench` and share the report.
9. [ ] A pilot on an isolated network is acceptable to them.
10. [ ] There is a budget owner for this work (needed for H1/H2 evidence, not for H3).
11. [ ] Their current stack is one `weft-bench` can compare against now or soon.

## Candidate shortlist (archetypes)

None contacted. Evidence links are public documentation found by web search on 2026-10-29; they
show the workload exists in that community, not that the named organisation wants anything.
Strength is the drafter's judgement of fit to the profile above.

| # | Archetype | Example public orgs / projects | Why interpolation is central | Evidence | Fit |
|---|---|---|---|---|---|
| 1 | Accelerator / light-source control-system archives | EPICS Archiver Appliance community (built by SLAC, BNL, MSU/FRIB) | Process variables archived on change (monitor mode); analysis needs values on common time bases; millions of PVs, long history | [Archiver user guide](https://epicsarchiver.readthedocs.io/en/stable/user/userguide.html), [SLAC archiver notes](https://confluence.slac.stanford.edu/x/dgsqBw) | Strong (workload), weak (payer) |
| 2 | Process-industry OT data teams on historians | Users of PI, Canary, Ignition historians (no specific org identified) | Change-of-value tags; historians step or linearly interpolate without labelling inferred values | [Canary on COV trends](https://helpcenter.canarylabs.com/t/p8haxag/interpolation-of-trends) | Strong, and a payer segment (H1) |
| 3 | Grid forecasting | LF Energy OpenSTEF; Alliander (public OpenSTEF user) | Inputs reindexed to a 15-min grid; gaps detected and either filled or raised | [OpenSTEF data integration](https://d1mks5pm1xidwl.cloudfront.net/stable/draft_release_v4/user_guide/data_integration.html), [Alliander case](https://lfenergy.org/openstef-delivers-operational-forecasts-at-hundreds-of-grid-locations-for-alliander/) | Strong, possible payer (H2) |
| 4 | Building-automation analytics | Project Haystack community | Trend logs captured on change of value; `hisInterpolate` tag exists so consumers know how to reconstruct | [Haystack forum on hisInterpolate](https://project-haystack.org/forum/topic/573), [oBIX COV discussion](https://lists.oasis-open.org/archives/obix/201403/msg00017.html) | Strong, smooth physical signals |
| 5 | Hydrology monitoring | USGS Water Data users; Colorado's OpenCDSS TSTool | Irregular instantaneous values; an "e" (estimated) qualifier — the same idea as WeftDB's marking; tools convert to regular intervals | [USGS IV qualifiers](https://help.waterdata.usgs.gov/codes-and-parameters/instantaneous-value-qualification-code-uv_rmk_cd), [TSTool IV reader](https://opencdss.state.co.us/tstool/14.9.2/doc-user/command-ref/ReadUsgsNwisInstantaneous/ReadUsgsNwisInstantaneous/) | Strong (workload), weak (payer) |
| 6 | Weather-station data tooling | `wetterdienst` (open source), `worldmet`/openair (R), NOAA ISD consumers | Station gaps; temporal and multi-station interpolation are first-class features of these tools | [wetterdienst interpolation](https://wetterdienst.readthedocs.io/en/latest/usage/interpolation.html), [worldmet manual](https://openair-project.r-universe.dev/worldmet/doc/manual.html) | Strong, smooth signals; open-source partner |
| 7 | Air-quality sensor networks | OpenAQ, Safecast | Low-cost sensors with gaps, aggregated to hourly/daily; linear fill found best in one published study | [OpenAQ v3 via monetio](https://monetio.readthedocs.io/en/v1/reference/monetio/obs/openaq_v3/), [gap-fill study (RSC 2019)](https://pubs.rsc.org/en/content/articlelanding/2019/em/c8em00593a) | Medium; good linear-fill test, not spline |
| 8 | Amateur satellite telemetry | SatNOGS / Libre Space Foundation | Telemetry exists only during received passes; stored in InfluxDB + Grafana; sparse, bursty series | [SatNOGS dashboard announcement](https://community.libre.space/t/announcing-satnogs-dashboard/3112) | Medium; extreme irregularity, small volume |
| 9 | Robotics multi-rate sensor logs | ROS 2 users (message_filters) | Sensors at different rates aligned by approximate-time matching that drops messages; offline log analysis needs a common grid | [ROS 2 message_filters](https://docs.ros.org/en/rolling/p/message_filters/doc/index.html) | Medium; online sync is not a database job, offline bag analysis is |
| 10 | ML feature pipelines | Feast (open-source feature store) | Point-in-time (as-of) joins over time-series features; resampling done upstream in Spark/pandas | [Feast point-in-time joins](https://docs.feast.dev/getting-started/concepts/point-in-time-joins) | Medium; a possible integration, H4 payer |
| 11 | ICU research datasets | PhysioNet MIMIC-IV research users | Vitals irregular; papers resample to 10-min or hourly grids with forward fill and capped gaps | [OmniTFT (arXiv 2511.19485)](https://arxiv.org/pdf/2511.19485), [LoS TPC (arXiv 2007.09483)](https://arxiv.org/pdf/2007.09483) | Medium workload, high compliance cost; research only |
| 12 | Ocean profiling floats | Argo programme | Pressures sampled irregularly per float and profile | [Argo interpolation (arXiv 1711.00460)](https://arxiv.org/pdf/1711.00460) | Weak: gaps are mostly vertical/spatial, not time |

Not shortlisted: market-data and crypto desks. The project's own corpus is prices, linear beats
spline there, and the wedge for that segment (exact `twa`, provenance) is H5, the weakest
hypothesis. Revisit after the external-engine TWA comparison exists.

## Public corpora that stand in for a partner's workload

A dataset is not a partner: it says nothing about budget. But until a partner shares data, these
give `weft-bench --gap-fill` real gappy series of the kinds above. Licences were checked on
2026-10-29 from the publishers' pages where noted; confirm each before vendoring a corpus.

| Corpus | Shape | Licence | Stands in for |
| --- | --- | --- | --- |
| [UCI Beijing Multi-Site Air Quality](https://archive.ics.uci.edu/dataset/501/beijing+multi+site+air+quality+data) | Hourly, 12 sites, 2013–2017, 420,768 rows, real `NA` gaps | CC BY 4.0 (UCI page) | Monitoring networks (#5); closest format to the UCI loader the bench already has |
| [NOAA NDBC standard met](https://www.ndbc.noaa.gov/historical_data.shtml) ([cleaned](https://data.ioos.us/dataset/ndbc-standard-meteorological-buoy-data-1970-present)) | Hourly / 10-min per buoy, 1970– | US federal; licence not stated on the pages read | Smooth physical signal (water temperature, pressure) for the spline test |
| [USGS NWIS instantaneous values](https://nwis.waterservices.usgs.gov/docs/instantaneous-values/) | Usually 15-min, 2007–, outage gaps | US federal; licence not stated on the pages read | Hydrology (#4); smooth streamflow and water temperature |
| [NREL SRRL BMS](https://catalog.data.gov/dataset/nrel-solar-radiation-research-laboratory-srrl-baseline-measurement-system-bms-golden-color-4ffaa) | 1-min irradiance, 130+ channels, 1981– | Catalog records disagree (CC BY vs CC0) | Smooth on clear days, not on cloudy ones: both sides of the spline claim |
| [PhysioNet Challenge 2012](https://physionet.org/content/challenge-2012/1.0.0/) | 12,000 ICU stays, 37 variables, irregular | ODC-By 1.0, open access (PhysioNet page) | Truly irregular sampling (#11) without credentialed access |
| [Petrobras 3W](https://github.com/petrobras/3W) | 1 Hz well sensors, frozen/missing variables left in | CC BY 4.0 for v2 per secondary sources; older fork is MIT | Industrial historian exports (#2) |
| [Building Data Genome 2](https://github.com/buds-lab/building-data-genome-project-2) | Hourly, 3,053 meters, 2016–2017 | CC BY-SA 4.0 (repository LICENSE) | Metering (#3); share-alike binds any derived corpus that ships |

Suggested order: UCI Beijing first (permissive, near-zero loader work), NDBC water temperature as
the smooth-signal test, 3W for H1.

## Suggested mix (operator decides)

A plausible 3–5, if the operator agrees with the reasoning: one of #2 or #3 (a segment that could
pay, tests H1/H2), one of #4, #5 or #6 (a smooth physical-signal corpus for the spline test), and
optionally #1 (the most interpolation-central archive workload). #10 is an integration
conversation rather than a partner. This is a suggestion, not a selection.

## Process

1. Operator picks candidates and reaches out as Basic Automation (discovery only until it is incorporated).
2. First contact asks for the workload, not a meeting about WeftDB: what they resample, how, and
   whether a sample dataset is public.
3. Score against the checklist after the first call; record score and date.
4. Run `weft-bench --gap-fill` on their data (or a public dataset of the same kind) with linear,
   previous-value and cubic fills, scored against held-out real values. Report losses.
5. Send the report; run the discovery script (WILLINGNESS-TO-PAY.md); record the answers.
6. After three months, record per partner: confirm / kill evidence for the relevant hypothesis.

Record each step as a dated entry. Do not record personal contact details in the repository.

## Needs human

- Choose the 3–5 candidates, and whether any listed archetype should be dropped or added.
- Whether a partner agreement or NDA is needed for data. (Decided: the operator makes contact as Basic Automation, which is not yet incorporated, so calls are discovery only; see WILLINGNESS-TO-PAY.md.)
- Whether partners are offered anything commercial (founding pricing, paid support credit).
- Where partner data may be stored: it must not enter this repository; `.gitignore` covers
  `database/datasets`, but a policy should say so.
- Whether to publish anonymised partner results, and the review step before publishing.
- (Decided) A pilot without TLS or auth is acceptable as an isolated technical test now, not for 1.0.
- Where this document should live: at the root until the site has a page for it, as for
  WILLINGNESS-TO-PAY.md (DECISIONS.md, 2026-10-29).
