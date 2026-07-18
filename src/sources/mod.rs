use anyhow::Result;
use async_trait::async_trait;
use serde::Serialize;
use std::fmt::Debug;

use crate::transform::types::{Product, Source};

use self::datatronic::DatatronicSource;
use self::gigantti::GiganttiSource;
use self::jimms::JimmsSource;
use self::multitronic::MultitronicSource;
use self::power::PowerSource;
use self::proshop::ProshopSource;
use self::verkkokauppa::VerkkokauppaSource;

pub mod datatronic;
pub mod gigantti;
pub mod jimms;
pub mod jsonld;
pub mod multitronic;
pub mod power;
pub mod proshop;
pub mod verkkokauppa;

#[async_trait]
pub trait RetailerSource: Debug + Send + Sync {
    fn source(&self) -> Source;
    async fn search(&self, query: &str, limit: usize) -> Result<Vec<Product>>;
    async fn get_product(&self, product_id: &str) -> Result<Option<Product>>;
    async fn get_category_products(&self, category_id: &str, limit: usize) -> Result<Vec<Product>>;
    fn extract_id_from_url(url: &str) -> Option<String>;
}

/// What a retailer actually supports, and under what terms.
///
/// This is reported by `hinta sources --json` so an agent can decide where to
/// route a query without discovering the limits through failed requests.
#[derive(Debug, Clone, Serialize)]
pub struct SourceInfo {
    pub id: &'static str,
    pub domain: &'static str,
    /// Whether text search is reachable from a plain HTTP client.
    pub search: bool,
    /// Whether a single product can be fetched by id.
    pub product_lookup: bool,
    /// Whether search results carry an EAN, which drives cross-retailer matching.
    pub ean_in_search: bool,
    pub transport: &'static str,
    pub robots: &'static str,
    pub notes: &'static str,
}

#[derive(Debug)]
pub enum RetailerSourceEnum {
    Jimms(JimmsSource),
    Proshop(ProshopSource),
    Gigantti(GiganttiSource),
    Multitronic(MultitronicSource),
    Datatronic(DatatronicSource),
    Verkkokauppa(VerkkokauppaSource),
    Power(PowerSource),
}

macro_rules! dispatch {
    ($self:ident, $source:ident => $body:expr) => {
        match $self {
            Self::Jimms($source) => $body,
            Self::Proshop($source) => $body,
            Self::Gigantti($source) => $body,
            Self::Multitronic($source) => $body,
            Self::Datatronic($source) => $body,
            Self::Verkkokauppa($source) => $body,
            Self::Power($source) => $body,
        }
    };
}

impl RetailerSourceEnum {
    pub fn source(&self) -> Source {
        dispatch!(self, s => s.source())
    }

    pub async fn search(&self, query: &str, limit: usize) -> Result<Vec<Product>> {
        dispatch!(self, s => s.search(query, limit).await)
    }

    pub async fn get_product(&self, product_id: &str) -> Result<Option<Product>> {
        dispatch!(self, s => s.get_product(product_id).await)
    }

    pub async fn get_category_products(
        &self,
        category_id: &str,
        limit: usize,
    ) -> Result<Vec<Product>> {
        dispatch!(self, s => s.get_category_products(category_id, limit).await)
    }

    pub fn extract_id_from_url(&self, url: &str) -> Option<String> {
        match self {
            Self::Jimms(_) => JimmsSource::extract_id_from_url(url),
            Self::Proshop(_) => ProshopSource::extract_id_from_url(url),
            Self::Gigantti(_) => GiganttiSource::extract_id_from_url(url),
            Self::Multitronic(_) => MultitronicSource::extract_id_from_url(url),
            Self::Datatronic(_) => DatatronicSource::extract_id_from_url(url),
            Self::Verkkokauppa(_) => VerkkokauppaSource::extract_id_from_url(url),
            Self::Power(_) => PowerSource::extract_id_from_url(url),
        }
    }

    pub fn info(&self) -> SourceInfo {
        info_for(&self.source())
    }

    /// Whether this source can serve a text search, used to decide which
    /// retailers `search` and `compare` fan out to.
    pub fn supports_search(&self) -> bool {
        self.info().search
    }
}

pub fn info_for(source: &Source) -> SourceInfo {
    match source {
        Source::Datatronic => SourceInfo {
            id: "datatronic",
            domain: "www.datatronic.fi",
            search: true,
            product_lookup: true,
            ean_in_search: false,
            transport: "PrestaShop server-rendered HTML",
            robots: "search permitted",
            notes: "EAN and brand come from the product page, not from search results",
        },
        Source::Verkkokauppa => SourceInfo {
            id: "verkkokauppa",
            domain: "www.verkkokauppa.com",
            search: true,
            product_lookup: true,
            ean_in_search: true,
            transport: "JSON:API on search.service.verkkokauppa.com",
            robots: "permitted; the API hosts publish no robots.txt and are separate from www",
            notes: "stock comes from a second availability call, batched 48 ids at a time",
        },
        Source::Power => SourceInfo {
            id: "power",
            domain: "www.power.fi",
            search: true,
            product_lookup: true,
            ean_in_search: true,
            transport: "JSON API at /api/v2/productlists",
            robots: "API path not disallowed, though /search/ and /haku/ are",
            notes: "consumer-electronics catalogue; carries few bare PC components",
        },
        Source::Jimms => SourceInfo {
            id: "jimms",
            domain: "www.jimms.fi",
            search: true,
            product_lookup: true,
            ean_in_search: false,
            transport: "JSON API at /api/product/newbetasearch",
            robots: "DISALLOWED: robots.txt excludes /api/*",
            notes: "EAN requires a product-page fetch; a delisted product answers 410",
        },
        Source::Multitronic => SourceInfo {
            id: "multitronic",
            domain: "www.multitronic.fi",
            search: true,
            product_lookup: true,
            ean_in_search: false,
            transport: "form POST to /fi/search/gpl returning an HTML fragment",
            robots: "DISALLOWED for search; product pages are permitted",
            notes: "server caps page size at 24; product pages publish full JSON-LD",
        },
        Source::Proshop => SourceInfo {
            id: "proshop",
            domain: "www.proshop.fi",
            search: true,
            product_lookup: true,
            ean_in_search: false,
            transport: "server-rendered HTML; requires a rustls TLS fingerprint",
            robots: "search permitted",
            notes: "throttles per IP and extends the penalty while blocked — keep requests slow",
        },
        Source::Gigantti => SourceInfo {
            id: "gigantti",
            domain: "www.gigantti.fi",
            search: false,
            product_lookup: true,
            ean_in_search: false,
            transport: "product pages only; behind a Vercel bot challenge",
            robots: "DISALLOWED for /api/ and ?_rsc=; product pages are permitted",
            notes: "search is client-rendered and unreachable; the origin rejects non-crawler \
                    clients with HTTP 429, so even product lookups usually fail",
        },
    }
}

pub fn all_sources() -> Vec<RetailerSourceEnum> {
    vec![
        RetailerSourceEnum::Datatronic(DatatronicSource::new()),
        RetailerSourceEnum::Verkkokauppa(VerkkokauppaSource::new()),
        RetailerSourceEnum::Power(PowerSource::new()),
        RetailerSourceEnum::Jimms(JimmsSource::new()),
        RetailerSourceEnum::Multitronic(MultitronicSource::new()),
        RetailerSourceEnum::Proshop(ProshopSource::new()),
        RetailerSourceEnum::Gigantti(GiganttiSource::new()),
    ]
}

/// The sources a fan-out search or comparison should query.
pub fn searchable_sources() -> Vec<RetailerSourceEnum> {
    all_sources()
        .into_iter()
        .filter(RetailerSourceEnum::supports_search)
        .collect()
}

/// Maximum product pages fetched per enrichment pass.
///
/// Enrichment costs one request per listing, and Proshop throttles per IP, so
/// this is capped rather than proportional to the result count.
pub const ENRICH_BUDGET: usize = 12;

/// Fills in EANs by fetching product pages for listings whose retailer omits
/// them from search results.
///
/// A hard identifier turns a fuzzy name match into a certain one, which is what
/// makes a cross-retailer "cheapest price" trustworthy. Failures are ignored:
/// enrichment is an improvement, never a precondition.
pub async fn enrich_missing_eans(products: &mut [Product]) -> usize {
    let mut targets: Vec<usize> = products
        .iter()
        .enumerate()
        .filter(|(_, p)| p.ean.is_none() && info_for(&p.source).product_lookup)
        .map(|(i, _)| i)
        .collect();
    targets.truncate(ENRICH_BUDGET);

    let lookups = targets.into_iter().map(|index| {
        let source = source_for(&products[index].source);
        let id = products[index].id.clone();
        async move { (index, source.get_product(&id).await) }
    });
    let results = futures::future::join_all(lookups).await;

    let mut enriched = 0;
    for (index, result) in results {
        let Ok(Some(detail)) = result else { continue };
        if detail.ean.is_none() && detail.sku.is_none() {
            continue;
        }
        let product = &mut products[index];
        if product.ean.is_none() {
            product.ean = detail.ean;
        }
        if product.sku.is_none() {
            product.sku = detail.sku;
        }
        if product.brand.is_none() {
            product.brand = detail.brand;
        }
        enriched += 1;
    }
    enriched
}

pub fn source_for(source: &Source) -> RetailerSourceEnum {
    match source {
        Source::Jimms => RetailerSourceEnum::Jimms(JimmsSource::new()),
        Source::Proshop => RetailerSourceEnum::Proshop(ProshopSource::new()),
        Source::Gigantti => RetailerSourceEnum::Gigantti(GiganttiSource::new()),
        Source::Multitronic => RetailerSourceEnum::Multitronic(MultitronicSource::new()),
        Source::Datatronic => RetailerSourceEnum::Datatronic(DatatronicSource::new()),
        Source::Verkkokauppa => RetailerSourceEnum::Verkkokauppa(VerkkokauppaSource::new()),
        Source::Power => RetailerSourceEnum::Power(PowerSource::new()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_source_variant_is_registered() {
        let sources = all_sources();
        assert_eq!(sources.len(), 7);

        let all = [
            Source::Jimms,
            Source::Proshop,
            Source::Gigantti,
            Source::Multitronic,
            Source::Datatronic,
            Source::Verkkokauppa,
            Source::Power,
        ];
        for source in all {
            assert!(
                sources.iter().any(|s| s.source() == source),
                "{:?} missing from all_sources()",
                source
            );
        }
    }

    #[test]
    fn source_for_round_trips_every_variant() {
        for source in [
            Source::Jimms,
            Source::Proshop,
            Source::Gigantti,
            Source::Multitronic,
            Source::Datatronic,
            Source::Verkkokauppa,
            Source::Power,
        ] {
            assert_eq!(source_for(&source).source(), source);
        }
    }

    #[test]
    fn searchable_sources_exclude_gigantti() {
        let searchable: Vec<Source> = searchable_sources().iter().map(|s| s.source()).collect();
        assert!(!searchable.contains(&Source::Gigantti));
        assert!(searchable.contains(&Source::Verkkokauppa));
        assert!(searchable.contains(&Source::Power));
        assert_eq!(searchable.len(), 6);
    }

    #[test]
    fn every_info_entry_matches_its_source_metadata() {
        for source in all_sources() {
            let info = source.info();
            assert_eq!(info.domain, source.source().domain());
            assert!(!info.notes.is_empty());
            assert!(!info.robots.is_empty());
            assert!(!info.transport.is_empty());
        }
    }

    #[test]
    fn sources_disallowed_by_robots_say_so_prominently() {
        for source in [Source::Jimms, Source::Multitronic, Source::Gigantti] {
            assert!(
                info_for(&source).robots.contains("DISALLOWED"),
                "{:?} should flag its robots.txt restriction",
                source
            );
        }
    }
}
