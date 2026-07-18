use anyhow::{bail, Result};
use async_trait::async_trait;
use chrono::Utc;

use crate::http::{RetryPolicy, CHROME_UA};
use crate::transform::types::{Product, Source};

use super::{jsonld, RetailerSource};

const ORIGIN: &str = "https://www.gigantti.fi";

/// Overrides the User-Agent Gigantti is addressed with.
///
/// Gigantti fronts its whole origin with Vercel Attack Challenge Mode, which
/// serves HTTP 429 to every browser User-Agent and admits only a fixed list of
/// crawler identities. hinta ships an honest identity and reports the block
/// rather than impersonating a crawler; setting this variable is a deliberate
/// choice by the operator, not a default of the tool.
const UA_ENV: &str = "HINTA_GIGANTTI_UA";

#[derive(Debug)]
pub struct GiganttiSource {
    client: reqwest::Client,
    policy: RetryPolicy,
}

impl Default for GiganttiSource {
    fn default() -> Self {
        Self::new()
    }
}

impl GiganttiSource {
    pub fn new() -> Self {
        let user_agent = std::env::var(UA_ENV).unwrap_or_else(|_| CHROME_UA.to_string());
        Self {
            client: reqwest::Client::builder()
                .user_agent(user_agent)
                .timeout(std::time::Duration::from_secs(30))
                .build()
                .expect("client configuration is valid"),
            policy: RetryPolicy::new(
                2,
                std::time::Duration::from_millis(500),
                std::time::Duration::from_secs(5),
            ),
        }
    }
}

/// Recognises Vercel's bot challenge so the caller gets a truthful diagnosis
/// instead of a bare 429 that looks like rate limiting.
pub(crate) fn is_bot_challenge(status: u16, headers: &reqwest::header::HeaderMap) -> bool {
    headers
        .get("x-vercel-mitigated")
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.eq_ignore_ascii_case("challenge"))
        || (status == 429 && headers.contains_key("x-vercel-challenge-token"))
}

fn challenge_error() -> anyhow::Error {
    anyhow::anyhow!(
        "gigantti.fi is behind a Vercel bot challenge that rejects this client (HTTP 429).\n\
         This is not rate limiting — retrying or slowing down will not help, because the \
         challenge admits only allow-listed crawler identities.\n\
         Options: set {} to an identity the origin accepts (you are responsible for that \
         choice), drive a real browser, or request product-feed access from the retailer.",
        UA_ENV
    )
}

pub(crate) fn parse_product_page(html: &str, fallback_id: &str) -> Option<Product> {
    let ld = jsonld::find_product(html)?;
    let name = ld.name?;
    let id = ld.sku.clone().unwrap_or_else(|| fallback_id.to_string());

    Some(Product {
        id: id.clone(),
        name,
        price_euro: ld.price_euro.unwrap_or(0.0),
        source: Source::Gigantti,
        // The canonical URL comes back from the redirect, which fixes the
        // slug-less URL that used to be stored.
        url: ld
            .url
            .unwrap_or_else(|| format!("{}/product/{}", ORIGIN, id)),
        image_url: ld.image,
        in_stock: ld.in_stock,
        ean: ld.gtin,
        sku: ld.mpn,
        brand: ld.brand,
        scraped_at: Utc::now(),
    })
}

#[async_trait]
impl RetailerSource for GiganttiSource {
    fn source(&self) -> Source {
        Source::Gigantti
    }

    /// Gigantti has no reachable search: the results page is client-rendered and
    /// ships zero products in its HTML, while `robots.txt` disallows both the
    /// JSON API (`/api/`) and the RSC payload (`/*?_rsc=*`) that carry them.
    async fn search(&self, _query: &str, _limit: usize) -> Result<Vec<Product>> {
        bail!(
            "gigantti.fi search is not available over HTTP.\n\
             The search page renders results client-side, and robots.txt disallows the \
             /api/ and ?_rsc= endpoints that serve them.\n\
             Individual products still work: hinta product <id> --source gigantti.\n\
             For catalogue-wide coverage the sanctioned route is the product sitemap at \
             {}/sitemaps/OCFIGIG.pdp.index.sitemap.xml",
            ORIGIN
        )
    }

    async fn get_product(&self, product_id: &str) -> Result<Option<Product>> {
        let (url, id) = if product_id.starts_with("http") {
            let id = Self::extract_id_from_url(product_id).unwrap_or_default();
            (product_id.to_string(), id)
        } else {
            (
                format!("{}/p/{}", ORIGIN, product_id),
                product_id.to_string(),
            )
        };

        let response =
            crate::http::get_with_retry(&self.client, &url, self.policy, Some(ORIGIN)).await?;
        let status = response.status();

        if is_bot_challenge(status.as_u16(), response.headers()) {
            return Err(challenge_error());
        }
        if status.as_u16() == 404 {
            return Ok(None);
        }
        if !status.is_success() {
            bail!("gigantti.fi product page returned HTTP {}", status.as_u16());
        }
        Ok(parse_product_page(&response.text().await?, &id))
    }

    async fn get_category_products(&self, _category_id: &str, _limit: usize) -> Result<Vec<Product>> {
        bail!("gigantti.fi category listings are client-rendered and not reachable over HTTP")
    }

    fn extract_id_from_url(url: &str) -> Option<String> {
        let trimmed = url.split(['?', '#']).next()?.trim_end_matches('/');
        let last = trimmed.rsplit('/').next()?;
        (!last.is_empty() && last.chars().any(|c| c.is_ascii_alphanumeric()))
            .then(|| last.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use reqwest::header::{HeaderMap, HeaderValue};

    const PDP_LD: &str = r#"<html><head><script type="application/ld+json">{
      "@context":"https://schema.org","@type":"Product",
      "name":"Intel Core i5-9400F prosessori",
      "image":"https://next-media.elkjop.com/image/184133/intel-core-i5-9400f.jpg",
      "url":"https://www.gigantti.fi/product/tietokonekomponentit/intel-core-i5-9400f/184133",
      "gtin":"5032037150354","mpn":"BX80684I59400F","sku":"184133",
      "brand":{"@type":"Brand","name":"Intel"},
      "offers":[
        {"@type":"Offer","name":"Standard Price","price":"164.99","priceCurrency":"EUR",
         "availability":"https://schema.org/SoldOut",
         "eligibleCustomerType":{"@type":"BusinessEntityType","@id":"https://schema.org/Public"},
         "priceSpecification":[{"@type":"UnitPriceSpecification","price":"131.47",
                                "valueAddedTaxIncluded":false}]},
        {"@type":"Offer","name":"Business Price (Excl. VAT)","price":"131.47",
         "availability":"https://schema.org/SoldOut",
         "eligibleCustomerType":{"@type":"BusinessEntityType","@id":"https://schema.org/Business"}}
      ]}</script></head><body></body></html>"#;

    #[test]
    fn parses_the_product_page_json_ld() {
        let product = parse_product_page(PDP_LD, "184133").unwrap();
        assert_eq!(product.id, "184133");
        assert_eq!(product.name, "Intel Core i5-9400F prosessori");
        assert_eq!(product.ean.as_deref(), Some("5032037150354"));
        assert_eq!(product.sku.as_deref(), Some("BX80684I59400F"));
        assert_eq!(product.brand.as_deref(), Some("Intel"));
        assert_eq!(product.in_stock, Some(false));
    }

    #[test]
    fn prefers_the_consumer_price_over_the_business_price() {
        let product = parse_product_page(PDP_LD, "184133").unwrap();
        assert_eq!(product.price_euro, 164.99);
    }

    #[test]
    fn stores_the_canonical_url_from_the_payload() {
        let product = parse_product_page(PDP_LD, "184133").unwrap();
        assert_eq!(
            product.url,
            "https://www.gigantti.fi/product/tietokonekomponentit/intel-core-i5-9400f/184133"
        );
    }

    #[test]
    fn tolerates_a_non_numeric_sku_and_a_null_price() {
        let html = r#"<script type="application/ld+json">{"@type":"Product",
          "name":"Lego set","sku":"SWININJAGO","offers":[{"@type":"Offer","price":null}]}</script>"#;
        let product = parse_product_page(html, "fallback").unwrap();
        assert_eq!(product.id, "SWININJAGO");
        assert_eq!(product.price_euro, 0.0);
    }

    #[test]
    fn falls_back_to_the_requested_id_when_the_payload_has_no_sku() {
        let html = r#"<script type="application/ld+json">{"@type":"Product",
          "name":"Thing"}</script>"#;
        assert_eq!(parse_product_page(html, "999").unwrap().id, "999");
    }

    #[test]
    fn a_page_without_json_ld_yields_nothing() {
        assert!(parse_product_page("<html><body>challenge</body></html>", "1").is_none());
    }

    #[test]
    fn detects_the_vercel_challenge() {
        let mut headers = HeaderMap::new();
        headers.insert("x-vercel-mitigated", HeaderValue::from_static("challenge"));
        assert!(is_bot_challenge(429, &headers));
        assert!(is_bot_challenge(200, &headers));

        let mut token_only = HeaderMap::new();
        token_only.insert("x-vercel-challenge-token", HeaderValue::from_static("abc"));
        assert!(is_bot_challenge(429, &token_only));

        assert!(!is_bot_challenge(429, &HeaderMap::new()));
        assert!(!is_bot_challenge(200, &HeaderMap::new()));
    }

    #[test]
    fn extracts_the_id_from_a_product_url() {
        assert_eq!(
            GiganttiSource::extract_id_from_url(
                "https://www.gigantti.fi/product/gaming/pelit/sackboy/220314"
            ),
            Some("220314".to_string())
        );
        assert_eq!(
            GiganttiSource::extract_id_from_url("https://www.gigantti.fi/p/SWININJAGO"),
            Some("SWININJAGO".to_string())
        );
        assert_eq!(
            GiganttiSource::extract_id_from_url("https://www.gigantti.fi/p/184133/?utm=x"),
            Some("184133".to_string())
        );
    }

    #[tokio::test]
    async fn search_reports_why_it_cannot_work() {
        let err = GiganttiSource::new().search("7800x3d", 5).await.unwrap_err().to_string();
        assert!(err.contains("client-side"), "unexpected error: {}", err);
        assert!(err.contains("robots.txt"), "unexpected error: {}", err);
    }
}
