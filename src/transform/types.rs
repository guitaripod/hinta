use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum Source {
    Jimms,
    Proshop,
    Gigantti,
    Multitronic,
    Datatronic,
    Verkkokauppa,
    Power,
}

impl Source {
    pub fn name(&self) -> &'static str {
        match self {
            Source::Jimms => "jimms.fi",
            Source::Proshop => "proshop.fi",
            Source::Gigantti => "gigantti.fi",
            Source::Multitronic => "multitronic.fi",
            Source::Datatronic => "datatronic.fi",
            Source::Verkkokauppa => "verkkokauppa.com",
            Source::Power => "power.fi",
        }
    }

    pub fn domain(&self) -> &'static str {
        match self {
            Source::Jimms => "www.jimms.fi",
            Source::Proshop => "www.proshop.fi",
            Source::Gigantti => "www.gigantti.fi",
            Source::Multitronic => "www.multitronic.fi",
            Source::Datatronic => "www.datatronic.fi",
            Source::Verkkokauppa => "www.verkkokauppa.com",
            Source::Power => "www.power.fi",
        }
    }
}

/// Resolves a user-supplied source name, accepting either the short id
/// (`jimms`) or the full domain (`jimms.fi`).
pub fn source_from_str(raw: &str) -> Option<Source> {
    let normalized = raw.trim().to_lowercase();
    let stem = normalized
        .strip_prefix("www.")
        .unwrap_or(&normalized)
        .split('.')
        .next()
        .unwrap_or(&normalized);

    match stem {
        "jimms" => Some(Source::Jimms),
        "proshop" => Some(Source::Proshop),
        "gigantti" => Some(Source::Gigantti),
        "multitronic" => Some(Source::Multitronic),
        "datatronic" => Some(Source::Datatronic),
        "verkkokauppa" => Some(Source::Verkkokauppa),
        "power" => Some(Source::Power),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn source_from_str_accepts_ids_domains_and_casing() {
        assert_eq!(source_from_str("jimms"), Some(Source::Jimms));
        assert_eq!(source_from_str("Jimms"), Some(Source::Jimms));
        assert_eq!(source_from_str("  JIMMS  "), Some(Source::Jimms));
        assert_eq!(source_from_str("jimms.fi"), Some(Source::Jimms));
        assert_eq!(source_from_str("www.jimms.fi"), Some(Source::Jimms));
        assert_eq!(source_from_str("verkkokauppa.com"), Some(Source::Verkkokauppa));
    }

    #[test]
    fn source_from_str_rejects_unknown_names() {
        assert_eq!(source_from_str("amazon"), None);
        assert_eq!(source_from_str(""), None);
    }

    #[test]
    fn every_source_round_trips_through_its_own_name() {
        for source in [
            Source::Jimms,
            Source::Proshop,
            Source::Gigantti,
            Source::Multitronic,
            Source::Datatronic,
            Source::Verkkokauppa,
            Source::Power,
        ] {
            assert_eq!(source_from_str(source.name()), Some(source.clone()));
            assert_eq!(source_from_str(source.domain()), Some(source));
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Product {
    pub id: String,
    pub name: String,
    pub price_euro: f64,
    pub source: Source,
    pub url: String,
    pub image_url: Option<String>,
    pub in_stock: Option<bool>,
    pub ean: Option<String>,
    /// Manufacturer part number or retailer article code, used to match the
    /// same product across retailers when no EAN is published.
    pub sku: Option<String>,
    pub brand: Option<String>,
    pub scraped_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PricePoint {
    pub product_id: String,
    pub source: Source,
    pub price_euro: f64,
    pub in_stock: Option<bool>,
    pub recorded_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SearchResult {
    pub products: Vec<Product>,
    pub total_hits: usize,
    pub sources_searched: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ComparisonResult {
    pub product_name: String,
    pub prices: Vec<PriceEntry>,
    pub cheapest: Option<PriceEntry>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PriceEntry {
    pub source: Source,
    pub price_euro: f64,
    pub url: String,
    pub in_stock: Option<bool>,
}
