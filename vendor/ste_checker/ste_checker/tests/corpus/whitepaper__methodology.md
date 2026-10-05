# Research methodology — source of truth

This is the canonical specification of **how we decide which developing economy
is the next Singapore**. It fixes (1) the factors we believe predict
developing-economy takeoff, (2) the procedural way to analyze each factor against
history (one python module per factor under `factors/`), and (3) the numeric
thresholds at which a factor declares a country "ready," plus the bar a factor
itself must clear to count as predictive.

Two anchors govern everything:

- **Theory:** Ray Dalio's *Changing World Order* — see `../refs/ray_dalio/`.
  The key idea we exploit: determinants are *sequenced*. Education, governance,
  rule of law, innovation and competitiveness **lead** an ascent; economic
  output, financial-center depth and reserve-currency status **lag** it. To pick
  the *next* riser we overweight the leaders and ignore the laggards.
- **Template:** Singapore 1965-1990 — see `../refs/countries/_singapore_template.md`.
  The concrete proof of the play: clean meritocratic governance + hard rule of
  law + forced high savings + relentless human-capital quality + aggressive
  FDI/trade openness, compounded for 25 years.

---

## 1. The factors

Ordered by weight. The first four are **leading institutional gates** — they
enter the composite *multiplicatively* (a near-floor score on any one caps the
whole composite, no matter how strong the rest). The remainder are **fuel** —
additive contributors. This gate structure is why a country with great
demographics and trade but a corruption score of 21 (Cambodia) cannot score
above the low-30s: the gate closes.

| # | Factor | Role | Dalio lead/lag |
|---|--------|------|----------------|
| 1 | Education / human-capital **quality** | GATE | long-leading |
| 2 | Governance & corruption control | GATE | leading/enabling |
| 3 | Rule of law / contract enforcement | GATE | leading/enabling |
| 4 | Cost competitiveness & export-manufacturing | GATE | leading |
| 5 | Innovation & technology adoption | fuel | early-leading |
| 6 | Investment & savings (capital formation) | fuel | mid-cycle |
| 7 | Demographic-dividend window | fuel/gate-lite | leading precondition |
| 8 | Macro & monetary stability | fuel | sound-money bridge |
| 9 | Trade openness & integration | fuel | mid-leading |
| 10 | Political stability & **developmental** continuity | fuel (interacts w/ #2) | leading |
| 11 | Infrastructure quality | fuel/discount | leading/coincident |

Deliberately **excluded as predictors** (they are lagging/coincident — they tell
you who *already* rose): current GDP level, share of world GDP, financial-center
depth, reserve-currency usage, military strength. We track them only as outcomes
to validate against.

Each factor below names: **why** it predicts (tied to Dalio + Singapore), the
**data source** (exact series codes), the **python module** that computes and
backtests it, and the **"ready" threshold**.

### 1. Education / human-capital quality — `factors/education.py` · GATE
*Why:* Dalio's single long-leading strength; the root from which innovation,
competitiveness, low corruption and rule of law grow. Singapore's obsessive
English-medium STEM schooling turned a poor entrepot into an MNC magnet. This is
where most candidates fail (Cambodia HCI 0.49; Philippines test scores near the
world floor despite high enrollment), so it discriminates hard. **Quality, not
seat-time** — high enrollment masking bottom-of-world PISA is a red flag.
*Data:* World Bank Human Capital Index; WDI `SE.TER.ENRR` (tertiary enrollment),
`SE.SEC.CUAT.UP.ZS` (upper-secondary completion), `SE.ADT.LITR.ZS` (literacy);
OECD PISA scores; **Barro-Lee** attainment for pre-1990 coverage of the
KOR/TWN/SGP takeoff labels.
*Threshold (ready):* tertiary enrollment >15% and rising, secondary completion
>60%, literacy >90% — **or** HCI ≥ 0.60. *Quality override:* harmonized test
scores below ~400 cap the factor regardless of enrollment.
*Unit:* index 0-1 (HCI); % enrollment / completion / literacy; PISA points.

### 2. Governance & corruption control — `factors/governance.py` · GATE
*Why:* the education→low-corruption→rule-of-law→cooperation chain makes clean,
capable governance the institutional gate of the ascent. Singapore's *entire*
comparative advantage was credible radical anti-corruption (CPI ~84 today). **No
country has reached high income from below the rule-of-law/corruption
threshold.** The most disqualifying factor.
*Data:* Transparency International **CPI** (0-100); World Bank WGI — Control of
Corruption `CC.EST`, Rule of Law `RL.EST`, Government Effectiveness `GE.EST`
(percentile ranks); **V-Dem** `v2x_rule` back to 1789 for the pre-1996 takeoff
windows, calibrated to CPI on the 1996-2020 overlap.
*Threshold (ready):* CPI ≥ 40 (or WGI control-of-corruption / rule-of-law
percentile ≥ 50th) and rising. **CPI < 30 is a near-automatic disqualifier**
regardless of other strengths.
*Unit:* CPI 0-100; WGI estimate (−2.5…+2.5) or 0-100 percentile.

### 3. Rule of law / contract enforcement — `factors/rule_of_law.py` · GATE
*Why:* impartial courts and enforceable contracts are what anchor the long-
horizon FDI a takeoff needs. Singapore's gold-standard commercial law was a core
draw. Often moves with #2 but measured separately because a country can have
moderate corruption yet unreliable courts (or vice-versa).
*Data:* World Justice Project **Rule of Law Index** (0-1, plus civil-justice and
regulatory-enforcement sub-scores); WGI `RL.EST` percentile; V-Dem `v2x_rule`,
`v2xnp_client` (clientelism) for history.
*Threshold (ready):* WJP overall ≥ 0.50 (and civil-justice ≥ 0.50), or WGI
rule-of-law percentile ≥ 50th, improving. Bottom-third (WJP < 0.45) is a gate
failure.
*Unit:* WJP 0-1; WGI percentile.

### 4. Cost competitiveness & export-manufacturing — `factors/competitiveness.py` · GATE
*Why:* Dalio's "inexpensive vs expensive" competitiveness is what makes revenues
exceed expenses and funds the whole rise — and it *leads*. The
Singapore/Korea/China/Vietnam template is unambiguously export-led: cheap,
capable, *improving-quality* manufacturing winning rising world-trade share.
This is where Vietnam shines and where commodity/garment-narrow economies only
half-qualify.
*Data:* WDI exports `NE.EXP.GNFS.ZS`; manufactures share `TX.VAL.MANF.ZS.UN`;
real effective exchange rate `PX.REX.REER` (over-valuation brake); country share
of world merchandise exports from `TX.VAL.MRCH.CD.WT` vs world total; WEF/IMD
competitiveness rankings.
*Threshold (ready):* manufactures > 50% of merchandise exports (or rising fast)
**and** the country's world-export share rising, with a competitive (not
over-valued) REER. Open-but-commodity/garment-narrow only partially qualifies.
*Unit:* % of GDP / % of merchandise exports / % share of world exports / REER.

### 5. Innovation & technology adoption — `factors/innovation.py`
*Why:* early-leading (second only to education); for developers the relevant
form is **absorption/adoption** and rising R&D and patenting off a low base —
the climb Korea/Taiwan made and that garment economies have not. Signals ability
to move *up* the value chain.
*Data:* WDI `GB.XPD.RSDV.GD.ZS` (R&D %GDP), `SP.POP.SCIE.RD.P6` (researchers/m),
`IP.PAT.RESD` (resident patents), `TX.VAL.TECH.MF.ZS` (high-tech exports % of
manufactured exports); WIPO Global Innovation Index; **Penn World Table** `rtfpna`
(TFP) as the outcome to validate against.
*Threshold (ready):* R&D/GDP > 0.5% and rising toward 1%; high-tech exports >10%
of manufactured exports and climbing; resident patents growing >15%/yr off any
base. *Score growth/acceleration, not level* (levels are tiny pre-takeoff).
*Unit:* %GDP (R&D); count & growth-rate (patents); % of manufactured exports.

### 6. Investment & savings (capital formation) — `factors/investment.py`
*Why:* the hard fuel of the income→investment→productivity loop. Every East
Asian miracle ran gross investment ~30-40% of GDP for decades. Necessary but not
sufficient — Cambodia has the *rate* but mis-allocated it into a property bubble,
which is why it must be paired with the governance and competitiveness gates.
*Data:* WDI `NE.GDI.FTOT.ZS` (gross fixed capital formation), `NE.GDI.TOTL.ZS`,
`NY.GDS.TOTL.ZS` (gross domestic savings), `BX.KLT.DINV.WD.GD.ZS` (FDI %GDP); PWT
`csh_i` for long history.
*Threshold (ready):* gross fixed capital formation > 25% of GDP **and** domestic
savings > 25% of GDP, sustained, with **ICOR < ~4** (= investment-share ÷
GDP-growth; a quality filter that separates productive Korea from bubble
Cambodia). Korea/China/Singapore ran 30-40%.
*Unit:* %GDP (investment, savings, FDI); dimensionless (ICOR).

### 7. Demographic-dividend window — `factors/demographics.py`
*Why:* the demand-side precondition behind every Asian takeoff — a large,
growing, low-dependency working-age cohort. But a dividend with thin human
capital has nowhere to flow (Cambodia, Philippines), so it counts only paired
with #1/#4. Predictive value is as a **gate-lite**: a *closing* window (China
now) caps upside; an *opening* one (Africa) is runway.
*Data:* WDI `SP.POP.DPND` (age-dependency), `SP.POP.1564.TO.ZS` (working-age %),
`SP.DYN.TFRT.IN` (fertility), `SP.POP.DPND.OL` (old-age dependency); UN WPP
median age + projected window-close year.
*Threshold (ready):* age-dependency < 60 and **falling**, working-age share
rising, fertility 1.8-3.0, with ≥15 years of window runway.
*Unit:* dependents per 100 working-age; % working-age; births/woman.

### 8. Macro & monetary stability — `factors/macro.py`
*Why:* Dalio's "sound money" is the bridge to becoming a capital magnet;
instability is a phase-3 decline symptom. Durable low inflation + stable currency
+ low/concessional debt are what make a country safe for long-horizon FDI. (The
factor where Cambodia genuinely scores well — the framework rewards real
strengths regardless of overall verdict.)
*Data:* WDI `FP.CPI.TOTL.ZG` (inflation), `NY.GDP.DEFL.KD.ZG` (deflator),
`GC.DOD.TOTL.GD.ZS` (govt debt %GDP), `BN.CAB.XOKA.GD.ZS` (current account),
`FI.RES.TOTL.MO` (reserves in months of imports); IMF WEO for external-debt /
concessionality.
*Threshold (ready):* inflation < 8% and low-volatility (10-yr std), public debt
< 60% of GDP and stable/falling, reserves > 3 months of imports, no chronic twin
deficits.
*Unit:* % annual inflation; %GDP (debt, current account); months of imports.

### 9. Trade openness & integration — `factors/trade.py`
*Why:* mid-leading "share of world trade" — the channel through which
technology, capital and demand arrive. Combine with #4 so openness without
competitive manufactures (commodity/transit openness) isn't over-credited.
*Data:* WDI `NE.TRD.GNFS.ZS` (trade %GDP), `TG.VAL.TOTL.GD.ZS`; KOF Globalisation
Index (trade sub-index); WTO RTA database (FTA counts); `LP.LPI.OVRL.XQ` (LPI).
*Threshold (ready):* trade > 70% of GDP and rising, FTA coverage of major
markets, improving logistics (LPI > 3.0), **backed by manufactures** not transit.
*Unit:* %GDP (trade); 0-100 (KOF); LPI 1-5.

### 10. Political stability & developmental continuity — `factors/political.py`
*Why:* a 25-year miracle needs a 25-year policy horizon (Singapore's PAP, Korea's
Park, China's Deng). **But continuity predicts only when developmental, not
extractive** — so this factor is scored *multiplicatively with #2*: high
continuity × low corruption = developmental; high continuity × high corruption =
extractive trap (Cambodia's stable kleptocracy). Avoids rewarding stable
kleptocracies.
*Data:* WGI `PV.EST` (political stability); V-Dem `v2x_polyarchy`, `v2x_libdem`,
state-capacity (covers pre-1996 windows); Polity5 durability; Fragile States Index.
*Threshold (ready):* political-stability percentile > 50th and stable **and**
paired with passing governance (CPI ≥ 40). Continuity alone with CPI < 30 is
scored as an extractive trap, not a strength.
*Unit:* WGI estimate/percentile; V-Dem 0-1.

### 11. Infrastructure quality — `factors/infrastructure.py`
*Why:* the investment loop's physical output; also a discount on trade openness
(open but high-friction logistics is hollow). Leading/coincident.
*Data:* WDI `LP.LPI.OVRL.XQ` (Logistics Performance Index), `EG.ELC.ACCS.ZS`
(electricity access); WEF infrastructure pillar.
*Threshold (ready):* LPI > 3.0 and rising, electricity access > 90%.
*Unit:* LPI 1-5; % access.

---

## 2. Procedural analysis — one python module per factor

Layout:

```
src/
  factors/
    wdi.py              # shared World Bank/V-Dem/PWT data access + labelled takeoffs
    education.py        # factor 1
    governance.py       # factor 2
    rule_of_law.py      # factor 3
    competitiveness.py  # factor 4
    innovation.py       # factor 5
    investment.py       # factor 6
    demographics.py     # factor 7
    macro.py            # factor 8
    trade.py            # factor 9
    political.py        # factor 10
    infrastructure.py   # factor 11
  composite.py          # gate-and-fuel aggregation + bootstrap confidence bands
```

`wdi.py` is the only network boundary (see the file): `series(code)` returns a
long `(iso3, year, value)` panel for any WDI series; `takeoffs()` returns the
labelled historical takeoffs we predict against.

Every factor module exposes the same two-function contract:

- `score(iso3, year) -> float` in [0,1] — the factor's readiness for one country
  at one point in time, computed from its threshold rules above.
- `backtest() -> dict` — runs the validation in §3 against the labelled takeoffs
  and the control set, returning `{hit_rate, auc, spearman}`.

No per-factor helper sprawl: thresholds live as module constants, the data layer
is shared, and `composite.py` is the only consumer that knows about gates.

Run reproducibly via the flake: `nix develop -c python -m factors.education`
(each module is runnable as a script that prints its `backtest()` table).

## 3. What makes a factor "predictive" — validation & thresholds

**Ground truth** (`factors/wdi.py::takeoffs()`): the labelled historical
takeoffs — KOR 1962, TWN 1962, SGP 1965, CHN 1980, IRL 1990, POL 1992 (plus VNM
1990 as an **in-sample sanity check only**, never validation, since the thesis
predicts Vietnam). A *takeoff* = the start year of a sustained convergence run,
formally **≥ 4% real GDP-per-capita CAGR sustained ≥ 15 years, closing ≥ 20pp of
the gap to the US frontier**.

**Control set:** a matched panel of low/lower-middle-income countries over the
*same calendar windows* that did **not** take off (Philippines-1960s, much of
Latin America, Sub-Saharan peers). Each factor is judged on its ability to
*separate takeoffs from look-alike non-takeoffs* — not merely correlate with
growth.

**Snapshot rule:** every factor is evaluated at **start_year − 5** (and the
0…+10 window) to enforce that we test *prediction*, not coincidence. This is what
privileges the leading factors over coincident/lagging ones.

**Coverage splicing:** WDI/CPI/WGI post-date the 1962-65 Asian takeoffs, so the
backtest spine for those windows uses Barro-Lee (education), V-Dem `v2x_rule`
(governance), and Penn World Table (investment, TFP), calibrated to the modern
series on their overlap. Without this the most important templates are
unscoreable.

**The bar a factor must clear to be retained** (all three):

1. **Leave-one-out hit-rate ≥ 70%** — drop each takeoff, refit thresholds,
   re-predict the held-out one at the stated threshold.
2. **ROC-AUC ≥ 0.70** separating takeoffs from the control set.
3. **Spearman rank-correlation ≥ 0.4** between the factor's pre-takeoff value and
   the subsequent 20-year per-capita-growth / TFP outcome.

Factors failing all three are dropped. Factors passing AUC but failing hit-rate
are kept only as **modifiers**, not gates.

**Composite construction** (`composite.py`): leading institutional gates
(education, governance, rule of law, competitiveness) enter **multiplicatively**
(a near-floor score caps the composite — this is precisely why strong demand-side
factors cannot lift Cambodia past the low-30s); fuel factors (investment, trade,
demographics, macro) are **additive**. Weights are tuned by ridge/logit on the
takeoff-vs-control panel with **leave-one-takeoff-out** cross-validation to avoid
overfitting to seven positives. Each candidate's composite is reported with
**bootstrap confidence bands** over the weights; any country whose rank flips
under reasonable reweighting is flagged low-confidence.

**Honest caveat:** with only seven labelled positives we validate at the
**factor level** (does each indicator generalize across the seven, leave-one-out)
rather than claiming a precisely calibrated probability per candidate. The
composite is a *ranking* tool, not a probability.

---

## Status

- ✅ Factor set, sources, thresholds, validation design — fixed (this file).
- ✅ Shared data layer `factors/wdi.py` — written.
- ⬜ Per-factor modules — stubs in place; implement `score()` + `backtest()`.
- ⬜ `composite.py` gate-and-fuel aggregation.
- 🔄 Country dossiers (`../refs/countries/`) — being populated by research run.
