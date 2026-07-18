# hinta

An agent-native CLI for searching, comparing, and tracking prices across Finnish
electronics retailers. One query fans out to every reachable retailer, and a
matching engine groups the same product across them — so "who has the 990 Pro 2 TB
cheapest, in stock" is one command, not seven tabs.

Two binaries: `hinta` (CLI) and `hinta-mcp` (an MCP server exposing the same
capabilities to agents). SQLite-backed price history at `~/.local/share/hinta`.
Every command takes `--json`; diagnostics go to stderr and data to stdout, so
piping into `jq` is always safe.

## Retailers

| Retailer | Search | EAN in search | Transport |
|----------|:------:|:-------------:|-----------|
| Verkkokauppa.com | yes | yes | JSON:API |
| Power.fi | yes | yes | JSON API |
| Datatronic.fi | yes | no | PrestaShop HTML |
| Jimms.fi | yes | no | JSON API |
| Multitronic.fi | yes | no | form POST → HTML |
| Proshop.fi | yes | no | server-rendered HTML |
| Gigantti.fi | no | no | product pages only (behind a bot challenge) |

`hinta sources --json` reports each retailer's capabilities and its robots.txt
posture, so an agent can route around what it may not fetch.

## Build

```bash
cargo build --release
cargo test                # 183 tests, no network required
```

## Examples

```bash
hinta compare "samsung 990 pro" --enrich --json      # group across retailers
hinta search "televisio" --devices-only --min-inches 65 --in-stock
hinta compare "7800x3d" --multi-only                 # only products ≥2 retailers stock
hinta alert 12345 --source power --below 750         # notify on refresh when price drops
hinta sources --json
```

Both `search` and `compare` accept `--min-price`, `--max-price`, `--in-stock`,
`--min-inches`, `--max-inches`, `--brand`, and `--devices-only`. Filters run
before grouping, so the reported cheapest price always refers to a listing that
passed. `--enrich` spends one product-page fetch per listing to fill in EANs the
retailer omits from search, turning a fuzzy name match into a certain one.

## MCP

```json
{"mcpServers": {"hinta": {"command": "/path/to/hinta-mcp"}}}
```

Thirteen tools mirror the CLI, including the filter and enrich arguments.

## Matching

`compare` groups listings by evidence, strongest first. A checksum-validated EAN
is decisive in both directions. Four hard vetoes block a merge outright: different
EANs, different qualifiers (`ti`/`super`/`pro`/`evo`…), different measurements
(capacity and screen size, each normalized onto one scale so `1 Tt`, `1TB` and
`1000 GB` agree), and disjoint model tokens. Scoring uses containment rather than
Jaccard, because retailers describe the same product at wildly different verbosity.
The bias throughout is to prefer a false split to a false merge: showing one offer
too few beats advertising a cheapest price that does not exist. `CLAUDE.md` has the
full account.

## robots.txt

Search is permitted for Verkkokauppa, Power, Datatronic, and Proshop, and
disallowed for Jimms, Multitronic, and Gigantti — the tool reports this per
retailer rather than hiding it. Product-detail pages are permitted everywhere,
which is why `refresh`, the command that runs repeatedly, uses only product
lookups. Automating search against a disallowed source is a deliberate choice the
tool leaves to the operator.

## License

GPL-3.0-or-later. See [LICENSE](LICENSE).
