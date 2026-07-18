# Hinta — Agent-Native CLI for Finnish Electronics Retailers

Rust, 7908 lines, 176 tests. Two binaries: `hinta` (CLI) and `hinta-mcp` (MCP server). SQLite at `~/.local/share/hinta/hinta.db`.

## Quick start

```bash
cargo build --release
cargo test                                          # 176 tests, no network needed
./target/release/hinta compare "7800x3d" --enrich --json          # group across retailers
./target/release/hinta search "televisio" --devices-only --min-inches 65 --in-stock
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

## robots.txt posture

This matters more here than the technical barriers, and the tool is explicit about it rather than quiet.

- **Permitted:** Verkkokauppa (the API hosts are separate from `www` and publish no robots.txt), Power (`/api/` is not disallowed, though `/search/` is), Datatronic, Proshop.
- **Disallowed for search:** Jimms (`/api/*`), Multitronic (`/{lang}/search`), Gigantti (`/api/`, `?_rsc=`).

`hinta sources --json` reports this per retailer in the `robots` field, so an agent can route around it. Product-detail pages are permitted everywhere, which is why `refresh` — the command that runs repeatedly and most resembles crawling — uses only product lookups. Search stays interactive and user-initiated. If you automate search against a disallowed source, that is a deliberate decision; make it knowingly.

## The matching engine (`src/matching/`)

This is the heart of `compare`, and the part most worth understanding before changing.

Evidence is applied strongest-first. A validated EAN is decisive **in both directions** — equal EANs match, different EANs are incompatible no matter how similar the names.

Four hard vetoes, any of which blocks a merge outright:

1. **Different validated EANs.** GTINs are checksum-validated, so a retailer's junk internal reference cannot merge two unrelated products.
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

## Architecture

```
src/
  lib.rs               # library root; both binaries build on it (needed for tests)
  main.rs              # CLI (clap), 12 subcommands
  mcp_bin.rs           # MCP binary; logs to stderr so stdout stays pure JSON-RPC
  util.rs              # urlencode, parse_price, stock phrases, URL resolution
  http.rs              # browser headers, retry/backoff policy, Retry-After
  matching/mod.rs      # signatures, vetoes, scoring, clustering  ← read this first
  transform/types.rs   # Product, PricePoint, Source
  store/mod.rs         # SQLite, migrations, sighting-based price history
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
search <query>  [--limit N] [--source S] [FILTERS]
compare <query> [--limit N] [--threshold F] [--multi-only] [--enrich] [FILTERS]
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

## MCP tools (13)

`search`, `compare`, `get_product`, `track`, `untrack`, `list_tracked`, `price_history`, `refresh`, `sources`, `stats`, `set_alert`, `clear_alert`, `list_alerts`

`search` and `compare` take the same filter arguments as the CLI; `compare` also takes `enrich`.

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
```

`sku` and the `alerts` table are added by migration on open, so existing databases upgrade in place (covered by a test). `set_alert` also tracks the product, since an alert on something `refresh` never checks would never fire.

Data dir: `HINTA_DATA_DIR`, default `~/.local/share/hinta`.

## Testing

`cargo test` runs 176 tests with **no network access** — parsers are tested against captured payload fixtures, so the suite stays deterministic when a retailer changes its catalogue. Verify the offline guarantee with `unshare -r -n cargo test --lib`; a test that reaches the network is a bug, not a slow test. When adding a scraper, capture one real response and write the parser test from it rather than asserting against live data.

## Known gaps / next steps

- [ ] Gigantti search needs a real browser or retailer feed access; product lookup needs a reachable identity.
- [ ] Bulk catalogue ingest via sitemaps (Gigantti ~460k URLs, Proshop ~380k, Power 35k) would enable offline search, complete coverage for category sweeps, and EAN coverage without per-query enrichment. Gigantti's product pages are robots-permitted even though its search is not, so this is the only route to including it.
- [ ] Alerts are evaluated on `refresh`; there is no daemon. A cron entry plus `notify-send`/webhook on `alerts_triggered` would close that.
- [ ] Search relevance is each retailer's own. Datatronic answers unrelated queries with fallback products (`televisio` returns CPU coolers), which `--devices-only` does not catch because they are genuine devices.
- [ ] Power's price-history endpoint (`/api/v2/products/{id}/pricehistory`) could backfill history on first track.
