# Hinta — Agent-Native CLI for Finnish Electronics Retailers

Rust, 9032 lines, 196 tests. Two binaries: `hinta` (CLI) and `hinta-mcp` (MCP server). SQLite at `~/.local/share/hinta/hinta.db`.

## Quick start

```bash
cargo build --release
cargo test                                          # 196 tests, no network needed
./target/release/hinta compare "7800x3d" --enrich --json          # group across retailers
./target/release/hinta search "televisio" --devices-only --min-inches 65 --in-stock
./target/release/hinta ingest power --limit 500                    # pull a catalogue from the sitemap
./target/release/hinta search "samsung u80" --local               # search it offline
./target/release/hinta sources --json               # capabilities + robots status per retailer
```

## Retailer status — all verified live

Six of seven retailers serve search. Every entry below was confirmed against the live site.

| Retailer | Transport | Search | EAN in search | Notes |
|----------|-----------|:------:|:-------------:|-------|
| **Verkkokauppa.com** | JSON:API on `search.service.verkkokauppa.com` | yes | **yes** | Stock needs a second `availability` call |
| **Power.fi** | JSON API `/api/v2/productlists` | yes | **yes** | Appliance-oriented catalogue, few bare components |
| **Datatronic.fi** | PrestaShop SSR HTML | yes | no | EAN only on the product page |
| **Jimms.fi** | JSON API `/api/product/newbetasearch` | yes | no | Delisted products answer **410**, not 404 |
| **Multitronic.fi** | form POST `/fi/search/gpl` → HTML fragment | yes | no | Server caps page size at 24 |
| **Proshop.fi** | SSR HTML | yes | no | Throttles **per IP**; keep requests slow |
| **Gigantti.fi** | product pages only | **no** | no | Vercel bot challenge; see below |

### Things that will bite you again if you forget them

- **Proshop was never a Cloudflare problem.** The old diagnosis was wrong. Cloudflare fingerprints the TLS ClientHello, and reqwest's default `native-tls` backend gets challenged (403) where `rustls` passes. `Cargo.toml` pins `default-features = false` + `rustls-tls`, and `ProshopSource` calls `.use_rustls_tls()`. Do not "simplify" that away. curl succeeds with either backend, so **you cannot infer reqwest's behaviour from curl here**.
- **Verkkokauppa's search parameter is `filter[q]`, not `q`.** Sending `q=` returns HTTP 200 with the *unfiltered* catalogue — success-shaped and completely wrong. `sessionId` is also mandatory (any opaque string) or the API 400s.
- **Gigantti's 429 is not rate limiting.** It is Vercel Attack Challenge Mode, which admits only allow-listed crawler User-Agents; the first cold request 429s and `robots.txt` itself 429s. Retrying or slowing down cannot help, so the source fails fast on `x-vercel-mitigated: challenge` instead. Search is doubly closed: the results page is client-rendered, and `robots.txt` disallows both `/api/` and `?_rsc=`. Product pages parse fine *if* reachable. `HINTA_GIGANTTI_UA` overrides the User-Agent — the shipped default impersonates nobody, and Vercel labels crawler-UA requests from this network `x-vercel-bot-status: impersonation`, so setting it is the operator's call, not the tool's.
- **Jimms rejects an empty User-Agent with 403.** One must always be sent.
- **Multitronic's search never reaches the server as a query string** — JS rewrites the form to a URL fragment. The real endpoint is the `gpl` POST.
- **Unscoped `[itemprop="name"]` reads the breadcrumb, not the product.** Datatronic and Jimms emit the breadcrumb trail (`Etusivu` / `Jimms.fi`, then each category) as `itemprop=name` elements *before* the product's `<h1>`, so a first-match microdata lookup named every product "Etusivu"/"Jimms.fi". Both product parsers now prefer the `<h1>` title. Jimms also carries stock in a `<link href>` and the image in `<img src>` (not `content`), so its microdata reader tries a list of attributes. The unit fixtures ordered the real name first, which masked all of this — a reminder to capture the *breadcrumb* in a fixture, not just the happy path.

## robots.txt posture

This matters more here than the technical barriers, and the tool is explicit about it rather than quiet.

- **Permitted:** Verkkokauppa (the API hosts are separate from `www` and publish no robots.txt), Power (`/api/` is not disallowed, though `/search/` is), Datatronic, Proshop.
- **Disallowed for search:** Jimms (`/api/*`), Multitronic (`/{lang}/search`), Gigantti (`/api/`, `?_rsc=`).

`hinta sources --json` reports this per retailer in the `robots` field, so an agent can route around it. Product-detail pages are permitted everywhere, which is why `refresh` — the command that runs repeatedly and most resembles crawling — uses only product lookups. Search stays interactive and user-initiated. If you automate search against a disallowed source, that is a deliberate decision; make it knowingly.

## The matching engine (`src/matching/`)

This is the heart of `compare`, and the part most worth understanding before changing.

Evidence is applied strongest-first. A validated EAN is decisive **in both directions** — equal EANs match, different EANs are incompatible no matter how similar the names.

Four hard vetoes, any of which blocks a merge outright:

1. **Different validated EANs.** GTINs are checksum-validated, so a retailer's junk internal reference cannot merge two unrelated products. Checksum-valid *placeholders* whose body is a short repeating cycle (`5656565656562`, `0000000000000`) are also rejected — Multitronic ships one such dummy that passes the checksum, and trusting it would merge every product that reuses it.
2. **Different qualifiers.** `ti`, `super`, `xt`, `pro`, `evo`, `plus`, `max`, … plus heatsink wording. This is what keeps `RTX 4070` / `4070 Ti` / `4070 Super` apart, and `990 PRO` from `990 EVO Plus`. Deliberately excludes inconsistently stated packaging words like `WOF` or `boxed`, which would cause false splits.
3. **Different measurements.** Normalized onto one scale first, in two families:
   - **Capacity** — `1 Tt` (Finnish *teratavu*), `1TB` and `1000 GB` all become `cap:1000gb`.
   - **Screen size** — `55"`, `55 tuuman` and `55 inch` all become `55inch`. The inch mark has to be rewritten *before* tokenizing, or it is stripped as punctuation and leaves a bare `55` that discriminates nothing.

   **Neither is optional.** Without capacity normalization a 1 TB and a 4 TB drive merge; without screen size a 55" and a 65" television merge. Both bugs shipped and were caught only by running real queries.
4. **Disjoint model tokens.** `Ryzen 7 7800X3D` and `Ryzen 9 7950X` share every word except the one that matters.

Scoring uses **containment (overlap coefficient), not Jaccard**. Retailers describe the same product at wildly different verbosity, and Jaccard punishes that asymmetry because extra words inflate the union. Model similarity is additionally scaled by the **length of the longest shared model token**, so a shared `7800x3d` counts far more than a shared `am5` — that scaling is what stops a CPU merging with a motherboard that mentions the same socket.

Clustering is agglomerative (a listing joins only if compatible with *every* member, so contradictory EANs cannot chain transitively), followed by a **merge pass** that joins any two mutually compatible clusters. Without that pass, results depend on the order retailers happen to reply in, and one product splits across two groups.

Bias throughout: **prefer a false split to a false merge.** A false split shows one offer too few; a false merge advertises a cheapest price that does not exist.

Verified live: `compare "samsung 990 pro"` yields exactly 6 groups — 3 capacities × 2 heatsink variants — each with all 4 stocking retailers, across three naming conventions.

### Product kind, and why it exists

Searching `televisio` returns wall mounts, antenna cables and PC-assembly services alongside televisions. Every listing is classified `device` / `accessory` / `service`:

- **Size ranges mean accessory.** A listing quoting `32-70"` fits many devices, so it is a bracket rather than a device of any size. This is the general signal; the vocabulary lists are the backstop.
- **Finnish compounds the head noun onto the end** — `seinäteline` (wall mount), `antennikaapeli` (antenna cable), `kasauspalvelu` (assembly service). Those are matched as *substrings*; exact token matching silently fails on every compound.
- English words like `stand` are matched **whole**, because as a substring it hits `standard`.

`--devices-only` drops the rest. On `televisio` at limit 40 that removed 153 of 200 listings.

### Filters and attributes

The matcher already has to work out brand, capacity, screen size and qualifiers in order to compare listings, so those are exposed rather than recomputed: every group carries an `attributes` object, and `search`/`compare` accept `--min-price`, `--max-price`, `--in-stock`, `--min-inches`, `--max-inches`, `--brand`, `--devices-only`.

Filters run **before** grouping, so an excluded listing cannot drag an unrelated product into a group and the reported cheapest price always refers to something that passed. Both commands report `filtered_out` so "nothing matched your filter" is distinguishable from "the retailers have nothing". `--in-stock` treats *unknown* stock as failing: promising availability the tool cannot confirm is worse than showing one offer fewer.

### Enrichment

Datatronic, Jimms, Multitronic and Proshop omit EANs from search results. `--enrich` spends one product-page fetch per listing (capped at `ENRICH_BUDGET`, 12) to fill them in. Measured on `compare "7800x3d"`:

| | matched on | confidence | retailers | offers with EAN |
|---|---|---|---|---|
| without | sku | 0.90 | 4 | 1/5 |
| **with** | **ean** | **1.00** | **5** | **5/5** |

It converts a fuzzy match into a certain one *and* finds retailers that could not otherwise be grouped confidently. It is opt-in because it costs requests, and Proshop throttles per IP.

## Sitemap ingest and offline search (`src/sitemap/`)

`hinta ingest <retailer>` walks a retailer's sitemap and stores every product in the local database; `search`/`compare --local` then query it offline and rank by hinta's own relevance rather than the retailer's. This is the answer to three problems no other route solves: it is the only robots-compliant way into **Gigantti** (its product pages are permitted even though search is walled off), it lets us own ranking instead of inheriting a retailer's bad relevance (Datatronic answering `televisio` with CPU coolers), and it harvests EANs once at ingest instead of per-query `--enrich`.

**The load-bearing insight: ingest is a crawler, not a second parser.** Every retailer's `get_product` already turns a product URL into a full record, and every one accepts a full URL, so the driver is uniform: walk sitemap → keep product `<loc>`s → `source.get_product(loc)` → `record_sighting`. What differs per retailer is captured in an `IngestPlan` — the root URL, whether it is a flat `<urlset>` or a `<sitemapindex>`, which child sitemaps hold products, which `<loc>`s are products, whether `<lastmod>` is present, and any required env.

- **Plans live in `plan_for`**, verified against each live sitemap: Power (flat `/services/sitemap.xml`, `/p-<digits>/`, ~33k, has `lastmod`), Verkkokauppa (index, keep `products-1..12`, drop `-eol`/`-en`/`-sv`, ~55k), Multitronic (index, `sitemap_product_*` children, keep `/fi/` locs and drop sv/ru, ~100k), Proshop (index `sitemap1..19`, products are the locs whose **last segment is all digits** — its category pages like `/Kytkimet/Lenovo` end in a slug, and `extract_id_from_url` returns that slug, so it *cannot* be the product test — ~310k), Gigantti (`OCFIGIG.pdp-*` index, `/product/` locs, ~459k, has `lastmod`, **requires `HINTA_GIGANTTI_UA`** or it bails with the reason). **Datatronic and Jimms have no plan** — Datatronic's sitemap is frozen at 2021 (half its URLs are dead, its product pages carry no numeric id), and Jimms publishes no sitemap.
- **The XML parser is hand-rolled** (`parse_entries`), not a dependency: it pulls exact `<loc>`/`<lastmod>` pairs so `<image:loc>` is never mistaken for a product URL, associates each `<lastmod>` with the `<loc>` it follows via a forward-only two-pointer scan, and unescapes entities. Sitemaps are machine-generated and rigidly regular, which is why this is safe; it is covered by fixtures including the `<image:loc>` trap.
- **Incremental re-ingest** uses the `ingest_state` table (`source, url` → `lastmod`). When a plan has `per_url_lastmod`, a URL whose stored `lastmod` matches the sitemap is skipped without a fetch; a URL with no `lastmod` is always re-fetched. Verified live: a second `ingest power --limit 6` reports `skipped_unchanged` for the already-seen URLs and fetches the next ones.
- **Politeness.** Product pages are fetched serially with `--delay` (default 1s) because Proshop throttles per IP. With `--limit`, index crawls short-circuit after collecting ~4× the limit in product URLs, so a bounded test run does not download every child sitemap. The Gigantti ingest client sends *only* the crawler UA (no contradicting browser client-hints), matching what the origin admits.
- **Local search** (`Store::search_local`) narrows candidates with an AND of `LIKE` clauses (identifiers via an `OR` escape hatch), then re-ranks in Rust by `matching::name_relevance`, which canonicalizes tokens the same way `compare` does (so a query `1tb` still ranks a stored `1 TB`). It never touches the network.

## Architecture

```
src/
  lib.rs               # library root; both binaries build on it (needed for tests)
  main.rs              # CLI (clap), 12 subcommands
  mcp_bin.rs           # MCP binary; logs to stderr so stdout stays pure JSON-RPC
  util.rs              # urlencode, parse_price, stock phrases, URL resolution
  http.rs              # browser headers, retry/backoff policy, Retry-After
  matching/mod.rs      # signatures, vetoes, scoring, clustering  ← read this first
  sitemap/mod.rs       # sitemap XML parsing, per-retailer ingest plans, the ingest driver
  transform/types.rs   # Product, PricePoint, Source
  store/mod.rs         # SQLite, migrations, sighting-based price history, ingest state, local search
  sources/
    mod.rs             # RetailerSource trait, enum dispatch, SourceInfo registry
    jsonld.rs          # shared schema.org Product extractor
    {datatronic,verkkokauppa,power,jimms,multitronic,proshop,gigantti}.rs
  mcp/mod.rs           # JSON-RPC over stdio, 10 tools
```

### Design decisions worth keeping

- **`lib.rs` exists so the code is testable.** The crate was binary-only before, which is precisely why it had zero tests — `tests/` could not import anything.
- **Prices live in `price_history`, not `products`.** `Store::record_sighting` appends a point only when price or stock actually changed, so searching repeatedly does not bury real movements under duplicates. Reads join the latest point back on. (The old `product_from_row` hardcoded `price_euro: 0.0`, silently zeroing every price read back from SQLite.)
- **`upsert_product` COALESCEs identifiers.** A later sparse search result cannot erase an EAN learned from a detail page.
- **JSON-LD is preferred over CSS selectors** wherever a retailer publishes it — it carries EAN, MPN and brand, which is what matching needs, and it survives theme changes. `jsonld.rs` prefers the consumer (VAT-inclusive) offer when a retailer publishes both consumer and business prices.
- **Static dispatch via enum** avoids `Box<dyn>` and async-trait-object issues; the `dispatch!` macro removes the seven-arm repetition.
- **Errors are per-source and never fatal.** `search`/`compare` report an `errors` array alongside results, so one blocked retailer cannot mask the rest.

## CLI

```
search <query>  [--limit N] [--source S] [--local] [FILTERS]
compare <query> [--limit N] [--threshold F] [--multi-only] [--enrich] [--local] [FILTERS]
ingest <source|all> [--limit N] [--delay SECS] [--full]   # bulk-load a catalogue from its sitemap
product <id|url> --source S                   # exit 2 when not found
track / untrack <product_id> --source S
alert <product_id> --source S --below PRICE   # implies track
unalert <product_id> --source S
alerts | tracked | stats | sources
history <product_id> --source S [--limit N]
refresh [--delay SECS]                        # reports alerts_triggered
open <id|url> [--source S]
mcp

FILTERS: --min-price --max-price --in-stock --min-inches --max-inches --brand --devices-only
```

Every command takes `--json`. Diagnostics go to stderr, data to stdout, so piping into `jq` is always safe.

## MCP tools (14)

`search`, `compare`, `ingest`, `get_product`, `track`, `untrack`, `list_tracked`, `price_history`, `refresh`, `sources`, `stats`, `set_alert`, `clear_alert`, `list_alerts`

`search` and `compare` take the same filter arguments as the CLI, plus `local`; `compare` also takes `enrich`. `ingest` takes `source`/`limit`/`delay`/`full`.

```json
{"mcpServers": {"hinta": {"command": "/home/marcus/Dev/hinta/target/release/hinta-mcp"}}}
```

Protocol notes: notifications (no `id`) get **no response** — replying to one is a spec violation the old server committed. Unknown methods return `-32601`. Tool failures come back as `isError: true` in the content rather than as transport errors, so a blocked retailer is distinguishable from an empty result.

## Database schema

```sql
products (id, source, name, url, image_url, ean, sku, brand, first_seen, last_seen)
  PRIMARY KEY (id, source)
price_history (id INTEGER PK, product_id, source, price_euro, in_stock, recorded_at)
tracked (product_id, source, added_at)  PRIMARY KEY (product_id, source)
alerts  (product_id, source, target_price, created_at)  PRIMARY KEY (product_id, source)
ingest_state (source, url, lastmod, fetched_at)  PRIMARY KEY (source, url)
```

`sku`, the `alerts` table and the `ingest_state` table are added by migration on open, so existing databases upgrade in place (covered by a test). `set_alert` also tracks the product, since an alert on something `refresh` never checks would never fire. `ingest_state` records the `lastmod` each sitemap URL was last ingested at, so re-ingest can skip unchanged products.

Data dir: `HINTA_DATA_DIR`, default `~/.local/share/hinta`.

## Testing

`cargo test` runs 176 tests with **no network access** — parsers are tested against captured payload fixtures, so the suite stays deterministic when a retailer changes its catalogue. Verify the offline guarantee with `unshare -r -n cargo test --lib`; a test that reaches the network is a bug, not a slow test. When adding a scraper, capture one real response and write the parser test from it rather than asserting against live data.

## Known gaps / next steps

- [x] **Bulk catalogue ingest via sitemaps** — done (`src/sitemap/`). Enables offline `--local` search, EAN coverage without per-query enrichment, and the only robots-compliant route into Gigantti. A *full* ingest of the large catalogues (Gigantti ~459k, Proshop ~310k, Multitronic ~100k) has not been run here — it is a long, per-IP-throttled crawl the operator should schedule; the capability is built, tested, and verified on bounded runs.
- [ ] Gigantti live search still needs a real browser or retailer feed; ingest routes around it via product pages but only with `HINTA_GIGANTTI_UA` set (an impersonation choice the tool leaves to the operator).
- [ ] Ingest fetches product pages serially with a delay; it has no resume-across-interruptions beyond the `ingest_state` lastmod skip (which only helps sources that publish `<lastmod>` — Power, Gigantti). Proshop/Multitronic/Verkkokauppa re-walk fully each run.
- [ ] Local search narrows with `LIKE`, which is diacritic-insensitive only for ASCII; a Finnish-diacritic query token can miss in the SQL narrowing (the Rust re-rank folds correctly, but only over what SQL returned).
- [ ] Alerts are evaluated on `refresh`; there is no daemon. A cron entry plus `notify-send`/webhook on `alerts_triggered` would close that.
- [ ] Power's price-history endpoint (`/api/v2/products/{id}/pricehistory`) could backfill history on first track.
