//! Bulk catalogue ingest from retailer sitemaps.
//!
//! Every retailer's `get_product` already turns a product URL into a full record
//! — name, price, EAN, brand — so ingest is a crawler, not a second parser: walk
//! the sitemap, keep the product URLs, and hand each to the source that owns it.
//! What differs per retailer is only the sitemap's shape and which URLs are
//! products, captured in an [`IngestPlan`].

use anyhow::{bail, Result};
use serde::Serialize;
use std::collections::BTreeSet;
use std::time::Duration;

use crate::http::{browser_headers, get_text, RetryPolicy, CHROME_UA};
use crate::sources::source_for;
use crate::store::Store;
use crate::transform::types::Source;

/// One `<loc>` from a sitemap, with its `<lastmod>` when the retailer publishes it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SitemapEntry {
    pub loc: String,
    pub lastmod: Option<String>,
}

/// Extracts the `<loc>`/`<lastmod>` pairs from a sitemap document.
///
/// Handles both a `<sitemapindex>` of child sitemaps and a `<urlset>` of product
/// URLs, since both express entries as a `<loc>` followed by an optional
/// `<lastmod>`. The image extension's `<image:loc>` is ignored because it is a
/// different tag, and each `<lastmod>` is paired with the `<loc>` it follows.
pub fn parse_entries(xml: &str) -> Vec<SitemapEntry> {
    let loc_spans = tag_spans(xml, "loc");
    let lastmod_spans = tag_spans(xml, "lastmod");

    let mut entries = Vec::with_capacity(loc_spans.len());
    let mut lm = 0;
    for (idx, &(ls, le)) in loc_spans.iter().enumerate() {
        let loc = unescape(xml[ls..le].trim());
        if loc.is_empty() {
            continue;
        }
        let next_loc = loc_spans.get(idx + 1).map(|&(s, _)| s).unwrap_or(usize::MAX);
        // Both span lists are ascending, so the pointer only moves forward.
        while lm < lastmod_spans.len() && lastmod_spans[lm].0 <= le {
            lm += 1;
        }
        let lastmod = lastmod_spans
            .get(lm)
            .filter(|&&(ms, _)| ms < next_loc)
            .map(|&(ms, me)| unescape(xml[ms..me].trim()))
            .filter(|s| !s.is_empty());

        entries.push(SitemapEntry { loc, lastmod });
    }
    entries
}

/// The inner-text spans of every `<tag>…</tag>`, exact-matched so `<loc>` never
/// captures `<image:loc>`.
fn tag_spans(xml: &str, tag: &str) -> Vec<(usize, usize)> {
    let open = format!("<{}>", tag);
    let close = format!("</{}>", tag);
    let mut spans = Vec::new();
    let mut i = 0;
    while let Some(rel) = xml[i..].find(&open) {
        let start = i + rel + open.len();
        let Some(crel) = xml[start..].find(&close) else {
            break;
        };
        let end = start + crel;
        spans.push((start, end));
        i = end + close.len();
    }
    spans
}

/// Resolves the XML entities that appear in sitemap URLs. `&amp;` is expanded
/// last so an escaped entity such as `&amp;lt;` yields the literal `&lt;`.
fn unescape(s: &str) -> String {
    s.replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&apos;", "'")
        .replace("&#39;", "'")
        .replace("&amp;", "&")
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SitemapKind {
    /// A single `<urlset>` of product (and other) URLs.
    Flat,
    /// A `<sitemapindex>` whose children are `<urlset>` documents.
    Index,
}

/// How to ingest one retailer's catalogue from its sitemap.
#[derive(Clone)]
pub struct IngestPlan {
    pub source: Source,
    pub root: &'static str,
    pub kind: SitemapKind,
    /// For an index, which child `<loc>`s hold products (vs. category/CMS pages).
    pub child_is_product: fn(&str) -> bool,
    /// Which product-page `<loc>`s to keep — drops category listings, locale
    /// duplicates and discontinued products.
    pub url_is_product: fn(&str) -> bool,
    /// Whether `<url>` entries carry a `<lastmod>` usable for cheap incremental
    /// re-ingest.
    pub per_url_lastmod: bool,
    /// An environment variable that must be set before ingest is attempted —
    /// Gigantti's opt-in to a crawler identity, which is the operator's call.
    pub requires_env: Option<&'static str>,
}

fn all(_: &str) -> bool {
    true
}

/// The last path segment, ignoring a trailing slash and any query or fragment.
fn last_segment(url: &str) -> &str {
    url.split(['?', '#'])
        .next()
        .unwrap_or(url)
        .trim_end_matches('/')
        .rsplit('/')
        .next()
        .unwrap_or("")
}

/// Proshop product URLs end in a numeric id; its category and brand-filter pages
/// (`/Kytkimet/Lenovo`) end in a slug, so the trailing segment separates them.
fn proshop_product(url: &str) -> bool {
    let seg = last_segment(url);
    !seg.is_empty() && seg.chars().all(|c| c.is_ascii_digit())
}

/// Power product URLs carry a terminal `/p-<digits>/`; articles and store pages
/// do not.
fn power_product(url: &str) -> bool {
    url.split("/p-")
        .nth(1)
        .and_then(|rest| rest.chars().next())
        .is_some_and(|c| c.is_ascii_digit())
}

fn gigantti_product(url: &str) -> bool {
    url.contains("/product/")
}

/// Keeps the Finnish product page, dropping the `/sv/` and `/ru/` duplicates the
/// Multitronic sitemap lists for the same product.
fn multitronic_product(url: &str) -> bool {
    url.contains("/fi/products/")
}

fn verkkokauppa_product(url: &str) -> bool {
    url.contains("/fi/product/")
}

/// Verkkokauppa's index carries live product sitemaps (`products-1..12`), locale
/// variants (`-en`/`-sv`), discontinued products (`products-eol-*`) and CMS
/// files. Only the live Finnish product sitemaps are ingested.
fn verkkokauppa_child(loc: &str) -> bool {
    loc.contains("products-")
        && !loc.contains("products-eol")
        && !loc.contains("-en.xml")
        && !loc.contains("-sv.xml")
}

fn multitronic_child(loc: &str) -> bool {
    loc.contains("sitemap_product")
}

/// The ingest plan for a retailer, or `None` when no sitemap route exists.
///
/// Datatronic is excluded despite having a sitemap: it was generated in 2021 and
/// never regenerated, so roughly half its URLs are dead and its product pages
/// carry no numeric id to key on. Jimms publishes no sitemap at all.
pub fn plan_for(source: &Source) -> Option<IngestPlan> {
    let plan = match source {
        Source::Power => IngestPlan {
            source: Source::Power,
            root: "https://www.power.fi/services/sitemap.xml",
            kind: SitemapKind::Flat,
            child_is_product: all,
            url_is_product: power_product,
            per_url_lastmod: true,
            requires_env: None,
        },
        Source::Verkkokauppa => IngestPlan {
            source: Source::Verkkokauppa,
            root: "https://www.verkkokauppa.com/gsitemaps1/sitemap.xml",
            kind: SitemapKind::Index,
            child_is_product: verkkokauppa_child,
            url_is_product: verkkokauppa_product,
            per_url_lastmod: false,
            requires_env: None,
        },
        Source::Multitronic => IngestPlan {
            source: Source::Multitronic,
            root: "https://www.multitronic.fi/sitemap.xml",
            kind: SitemapKind::Index,
            child_is_product: multitronic_child,
            url_is_product: multitronic_product,
            per_url_lastmod: false,
            requires_env: None,
        },
        Source::Proshop => IngestPlan {
            source: Source::Proshop,
            root: "https://www.proshop.fi/sitemap.xml",
            kind: SitemapKind::Index,
            child_is_product: all,
            url_is_product: proshop_product,
            per_url_lastmod: false,
            requires_env: None,
        },
        Source::Gigantti => IngestPlan {
            source: Source::Gigantti,
            root: "https://www.gigantti.fi/sitemaps/OCFIGIG.pdp.index.sitemap.xml",
            kind: SitemapKind::Index,
            child_is_product: all,
            url_is_product: gigantti_product,
            per_url_lastmod: true,
            requires_env: Some("HINTA_GIGANTTI_UA"),
        },
        Source::Datatronic | Source::Jimms => return None,
    };
    Some(plan)
}

/// The retailers that can be ingested from a sitemap, in a sensible default order
/// (smallest and least rate-limited first).
pub fn ingestable_sources() -> Vec<Source> {
    [
        Source::Power,
        Source::Verkkokauppa,
        Source::Multitronic,
        Source::Proshop,
        Source::Gigantti,
    ]
    .into_iter()
    .filter(|s| plan_for(s).is_some())
    .collect()
}

#[derive(Debug, Clone)]
pub struct IngestOptions {
    /// Cap on product pages fetched this run; `None` ingests the whole catalogue.
    pub limit: Option<usize>,
    /// Pause between product fetches, to respect per-IP throttling.
    pub delay: Duration,
    /// Re-fetch every product even when its `lastmod` is unchanged.
    pub full: bool,
}

impl Default for IngestOptions {
    fn default() -> Self {
        Self {
            limit: None,
            delay: Duration::from_secs(1),
            full: false,
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct IngestReport {
    pub source: String,
    /// Product URLs discovered across the sitemap(s).
    pub product_urls: usize,
    /// Skipped because their `lastmod` was unchanged since the last ingest.
    pub skipped_unchanged: usize,
    /// Product pages actually fetched this run.
    pub fetched: usize,
    /// Records written to the database.
    pub recorded: usize,
    /// Of those fetched, how many carried an EAN.
    pub with_ean: usize,
    /// Product pages that errored (and were skipped).
    pub errors: usize,
}

/// Ingests a retailer's catalogue from its sitemap into the local database.
pub async fn ingest(store: &Store, source: &Source, opts: &IngestOptions) -> Result<IngestReport> {
    let plan = plan_for(source).ok_or_else(|| no_plan_error(source))?;

    if let Some(var) = plan.requires_env {
        if std::env::var(var).is_err() {
            bail!(
                "{} ingest requires {} to be set to a crawler identity the origin admits.\n\
                 Gigantti fronts its whole site with a Vercel bot challenge; hinta ships an \
                 honest identity and will not impersonate a crawler on its own — setting this \
                 is the operator's deliberate choice.",
                source.name(),
                var
            );
        }
    }

    let client = ingest_client(&plan)?;
    let soft_cap = discovery_cap(opts.limit, plan.per_url_lastmod, opts.full);
    let mut entries = discover(&client, &plan, source, soft_cap).await?;
    let product_urls = entries.len();

    let mut skipped_unchanged = 0;
    if plan.per_url_lastmod && !opts.full {
        entries.retain(|entry| match &entry.lastmod {
            Some(current) => {
                let seen = store.ingest_lastmod(source, &entry.loc).ok().flatten();
                let unchanged = seen.as_deref() == Some(current.as_str());
                if unchanged {
                    skipped_unchanged += 1;
                }
                !unchanged
            }
            None => true,
        });
    }

    if let Some(limit) = opts.limit {
        entries.truncate(limit);
    }

    let retailer = source_for(source);
    let mut report = IngestReport {
        source: source.name().to_string(),
        product_urls,
        skipped_unchanged,
        fetched: 0,
        recorded: 0,
        with_ean: 0,
        errors: 0,
    };

    for entry in &entries {
        match retailer.get_product(&entry.loc).await {
            Ok(Some(product)) => {
                report.fetched += 1;
                if product.ean.is_some() {
                    report.with_ean += 1;
                }
                if store.record_sighting(&product).is_ok() {
                    report.recorded += 1;
                }
                let _ = store.record_ingest(source, &entry.loc, entry.lastmod.as_deref());
            }
            // A delisted URL still gets recorded so an unchanged lastmod skips it
            // next time rather than re-fetching a 404 forever.
            Ok(None) => {
                let _ = store.record_ingest(source, &entry.loc, entry.lastmod.as_deref());
            }
            Err(_) => report.errors += 1,
        }

        if !opts.delay.is_zero() {
            tokio::time::sleep(opts.delay).await;
        }
    }

    Ok(report)
}

/// How many product URLs discovery may collect before stopping early.
///
/// A bounded run caps how much of an index we crawl just to find URLs — except
/// in incremental mode, where the `lastmod` skip needs the whole URL universe
/// each run to advance past the front slice it already ingested. Without the
/// exception, a nightly `ingest gigantti --limit N` would re-collect and re-skip
/// the same first child sitemap forever and never reach the rest of the catalogue.
fn discovery_cap(limit: Option<usize>, per_url_lastmod: bool, full: bool) -> Option<usize> {
    let incremental = per_url_lastmod && !full;
    match limit {
        Some(limit) if !incremental => Some(limit.saturating_mul(4).max(64)),
        _ => None,
    }
}

/// Walks the sitemap tree and returns the deduplicated product URLs.
async fn discover(
    client: &reqwest::Client,
    plan: &IngestPlan,
    source: &Source,
    soft_cap: Option<usize>,
) -> Result<Vec<SitemapEntry>> {
    let policy = RetryPolicy::new(3, Duration::from_secs(2), Duration::from_secs(30));
    let root_xml = get_text(client, plan.root, policy, None, source.name()).await?;
    let root = parse_entries(&root_xml);

    let mut products: Vec<SitemapEntry> = Vec::new();
    let mut seen: BTreeSet<String> = BTreeSet::new();
    let push = |entry: SitemapEntry, out: &mut Vec<SitemapEntry>, seen: &mut BTreeSet<String>| {
        if seen.insert(entry.loc.clone()) {
            out.push(entry);
        }
    };

    match plan.kind {
        SitemapKind::Flat => {
            for entry in root {
                if (plan.url_is_product)(&entry.loc) {
                    push(entry, &mut products, &mut seen);
                }
            }
        }
        SitemapKind::Index => {
            for child in root.into_iter().filter(|c| (plan.child_is_product)(&c.loc)) {
                let Ok(child_xml) = get_text(client, &child.loc, policy, None, source.name()).await
                else {
                    continue;
                };
                for entry in parse_entries(&child_xml) {
                    if (plan.url_is_product)(&entry.loc) {
                        push(entry, &mut products, &mut seen);
                    }
                }
                if soft_cap.is_some_and(|cap| products.len() >= cap) {
                    break;
                }
            }
        }
    }

    Ok(products)
}

/// Builds the HTTP client for fetching sitemaps.
///
/// When a crawler identity is required, only the User-Agent is sent — the
/// browser client hints would contradict it and defeat the point of setting it.
fn ingest_client(plan: &IngestPlan) -> Result<reqwest::Client> {
    let mut builder = reqwest::Client::builder()
        .use_rustls_tls()
        .cookie_store(true)
        .timeout(Duration::from_secs(60));

    builder = match plan.requires_env.and_then(|var| std::env::var(var).ok()) {
        Some(crawler_ua) => builder.user_agent(crawler_ua),
        None => builder
            .user_agent(CHROME_UA)
            .default_headers(browser_headers("fi-FI,fi;q=0.9")),
    };

    Ok(builder.build()?)
}

fn no_plan_error(source: &Source) -> anyhow::Error {
    match source {
        Source::Datatronic => anyhow::anyhow!(
            "datatronic has no sitemap ingest: its sitemap was generated in 2021 and never \
             regenerated, so most of its URLs are dead and its product pages carry no id to key on"
        ),
        Source::Jimms => anyhow::anyhow!(
            "jimms publishes no sitemap; individual products are reachable via `hinta product`, \
             but there is no catalogue to enumerate"
        ),
        other => anyhow::anyhow!("{} cannot be ingested from a sitemap", other.name()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const INDEX: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
    <sitemapindex xmlns="http://www.sitemaps.org/schemas/sitemap/0.9">
      <sitemap><loc>https://www.gigantti.fi/sitemaps/OCFIGIG.pdp-1.xml</loc>
        <lastmod>2026-07-17</lastmod></sitemap>
      <sitemap><loc>https://www.gigantti.fi/sitemaps/OCFIGIG.pdp-2.xml</loc></sitemap>
    </sitemapindex>"#;

    const URLSET: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
    <urlset xmlns="http://www.sitemaps.org/schemas/sitemap/0.9"
            xmlns:image="http://www.google.com/schemas/sitemap-image/1.1">
      <url>
        <loc>https://www.power.fi/tv-ja-audio/radiot/tivoli-model-one/p-102010/</loc>
        <lastmod>2026-07-14</lastmod><changefreq>weekly</changefreq><priority>0.5</priority>
        <image:image><image:loc>https://media.power-cdn.net/images/a.webp</image:loc></image:image>
      </url>
      <url>
        <loc>https://www.power.fi/artikkelit/ostajan-oppaat/</loc>
        <lastmod>2026-07-11</lastmod>
      </url>
      <url>
        <loc>https://www.power.fi/kodinkoneet/astianpesukone-x-y%C3%A4/p-9/</loc>
      </url>
    </urlset>"#;

    #[test]
    fn parses_an_index_and_pairs_lastmod_with_its_loc() {
        let entries = parse_entries(INDEX);
        assert_eq!(entries.len(), 2);
        assert_eq!(
            entries[0].loc,
            "https://www.gigantti.fi/sitemaps/OCFIGIG.pdp-1.xml"
        );
        assert_eq!(entries[0].lastmod.as_deref(), Some("2026-07-17"));
        assert_eq!(entries[1].lastmod, None, "the second child has no lastmod");
    }

    #[test]
    fn ignores_image_loc_and_reads_only_the_url_loc() {
        let entries = parse_entries(URLSET);
        assert_eq!(entries.len(), 3, "the image:loc must not become an entry");
        assert!(entries[0].loc.ends_with("/p-102010/"));
        assert_eq!(entries[0].lastmod.as_deref(), Some("2026-07-14"));
        // The entry with no lastmod does not steal the following entry's.
        assert_eq!(entries[2].lastmod, None);
    }

    #[test]
    fn unescapes_entities_in_a_loc() {
        let xml = r#"<urlset><url><loc>https://x.fi/a?b=1&amp;c=2</loc></url></urlset>"#;
        assert_eq!(parse_entries(xml)[0].loc, "https://x.fi/a?b=1&c=2");
    }

    #[test]
    fn an_empty_document_yields_no_entries() {
        assert!(parse_entries("<urlset></urlset>").is_empty());
        assert!(parse_entries("not xml at all").is_empty());
    }

    #[test]
    fn power_keeps_only_product_urls_from_its_flat_sitemap() {
        let plan = plan_for(&Source::Power).unwrap();
        assert_eq!(plan.kind, SitemapKind::Flat);
        assert!((plan.url_is_product)(
            "https://www.power.fi/tv-ja-audio/radiot/tivoli/p-102010/"
        ));
        assert!(!(plan.url_is_product)("https://www.power.fi/artikkelit/ostajan-oppaat/"));
        assert!(!(plan.url_is_product)("https://www.power.fi/myymalat/"));
    }

    #[test]
    fn proshop_distinguishes_products_from_category_and_brand_pages() {
        let plan = plan_for(&Source::Proshop).unwrap();
        assert!((plan.url_is_product)(
            "https://www.proshop.fi/Naeppaeimistoet/Logitech-K120/3290345"
        ));
        assert!(!(plan.url_is_product)("https://www.proshop.fi/Kytkimet/Lenovo"));
        assert!(!(plan.url_is_product)("https://www.proshop.fi/Kytkimet"));
    }

    #[test]
    fn multitronic_keeps_the_finnish_locale_only() {
        let plan = plan_for(&Source::Multitronic).unwrap();
        assert!((plan.url_is_product)("https://www.multitronic.fi/fi/products/3036"));
        assert!(!(plan.url_is_product)("https://www.multitronic.fi/sv/products/3036"));
        assert!(!(plan.url_is_product)("https://www.multitronic.fi/ru/products/3036"));
        assert!((plan.child_is_product)(
            "https://www.multitronic.fi/sitemap/fi/sitemap_product_120.xml"
        ));
        assert!(!(plan.child_is_product)(
            "https://www.multitronic.fi/sitemap/fi/sitemap_category.xml"
        ));
    }

    #[test]
    fn verkkokauppa_excludes_eol_and_locale_child_sitemaps() {
        let plan = plan_for(&Source::Verkkokauppa).unwrap();
        assert!((plan.child_is_product)("https://cdn.verkkokauppa.com/gsitemaps1/products-1.xml"));
        assert!((plan.child_is_product)("https://cdn.verkkokauppa.com/gsitemaps1/products-12.xml"));
        assert!(!(plan.child_is_product)(
            "https://cdn.verkkokauppa.com/gsitemaps1/products-eol-1.xml"
        ));
        assert!(!(plan.child_is_product)(
            "https://cdn.verkkokauppa.com/gsitemaps1/products-1-en.xml"
        ));
        assert!(!(plan.child_is_product)(
            "https://cdn.verkkokauppa.com/gsitemaps1/content.xml"
        ));
        assert!((plan.url_is_product)(
            "https://www.verkkokauppa.com/fi/product/19/DELTACO-Y-haarajohto"
        ));
    }

    #[test]
    fn gigantti_requires_an_opt_in_crawler_identity() {
        let plan = plan_for(&Source::Gigantti).unwrap();
        assert_eq!(plan.requires_env, Some("HINTA_GIGANTTI_UA"));
        assert!(plan.per_url_lastmod);
    }

    #[test]
    fn discovery_cap_lifts_the_cap_for_an_incremental_index_crawl() {
        // A full (non-incremental) bounded run caps discovery cost...
        assert_eq!(discovery_cap(Some(50), true, true), Some(200));
        assert_eq!(discovery_cap(Some(50), false, false), Some(200));
        assert_eq!(discovery_cap(Some(5), false, false), Some(64));
        // ...but an incremental lastmod crawl must see the whole universe each
        // run to advance, so its discovery is uncapped even with a limit.
        assert_eq!(discovery_cap(Some(50), true, false), None);
        // No limit is always uncapped.
        assert_eq!(discovery_cap(None, false, false), None);
        assert_eq!(discovery_cap(None, true, false), None);
    }

    #[test]
    fn retailers_without_a_sitemap_route_have_no_plan() {
        assert!(plan_for(&Source::Datatronic).is_none());
        assert!(plan_for(&Source::Jimms).is_none());
        assert_eq!(ingestable_sources().len(), 5);
        assert!(!ingestable_sources().contains(&Source::Jimms));
    }
}
