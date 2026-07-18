use anyhow::{bail, Result};
use async_trait::async_trait;
use chrono::Utc;
use serde::Deserialize;
use std::collections::HashMap;

use crate::http::{RetryPolicy, CHROME_UA};
use crate::transform::types::{Product, Source};

use super::RetailerSource;

const ORIGIN: &str = "https://www.verkkokauppa.com";
const SEARCH_API: &str = "https://search.service.verkkokauppa.com/fi/api/v1/product-search";
const PRODUCT_API: &str = "https://web-api.service.verkkokauppa.com/products";
const AVAILABILITY_API: &str = "https://product.service.verkkokauppa.com/fi/api/v1/availability";

/// The availability service is batched by their own frontend at 48 ids a call.
const AVAILABILITY_BATCH: usize = 48;

#[derive(Debug)]
pub struct VerkkokauppaSource {
    client: reqwest::Client,
    policy: RetryPolicy,
}

impl Default for VerkkokauppaSource {
    fn default() -> Self {
        Self::new()
    }
}

impl VerkkokauppaSource {
    pub fn new() -> Self {
        Self {
            client: reqwest::Client::builder()
                .user_agent(CHROME_UA)
                .timeout(std::time::Duration::from_secs(30))
                .build()
                .expect("static client configuration is valid"),
            policy: RetryPolicy::default(),
        }
    }
}

#[derive(Debug, Deserialize)]
struct SearchResponse {
    #[serde(default)]
    data: Vec<SearchItem>,
    #[serde(default)]
    included: Vec<Included>,
}

#[derive(Debug, Deserialize)]
struct SearchItem {
    id: String,
    attributes: SearchAttributes,
    #[serde(default)]
    relationships: Option<Relationships>,
}

#[derive(Debug, Deserialize)]
struct SearchAttributes {
    name: Option<String>,
    href: Option<String>,
    #[serde(default)]
    price: Option<Price>,
    #[serde(default)]
    images: Vec<Image>,
    #[serde(default)]
    articles: Vec<Article>,
}

#[derive(Debug, Deserialize)]
struct Price {
    current: Option<f64>,
}

#[derive(Debug, Deserialize)]
struct Image {
    orig: Option<String>,
}

#[derive(Debug, Deserialize)]
struct Article {
    #[serde(default)]
    eans: Vec<String>,
}

#[derive(Debug, Deserialize)]
struct Relationships {
    brand: Option<Relationship>,
}

#[derive(Debug, Deserialize)]
struct Relationship {
    data: Option<RelationshipData>,
}

#[derive(Debug, Deserialize)]
struct RelationshipData {
    id: String,
}

#[derive(Debug, Deserialize)]
struct Included {
    #[serde(rename = "type")]
    kind: String,
    id: String,
    attributes: IncludedAttributes,
}

#[derive(Debug, Deserialize)]
struct IncludedAttributes {
    name: Option<String>,
}

#[derive(Debug, Deserialize)]
struct Availability {
    pid: i64,
    status: Option<AvailabilityStatus>,
}

#[derive(Debug, Deserialize)]
struct AvailabilityStatus {
    schema: Option<String>,
}

pub(crate) fn parse_availability(body: &str) -> HashMap<String, bool> {
    serde_json::from_str::<Vec<Availability>>(body)
        .unwrap_or_default()
        .into_iter()
        .filter_map(|entry| {
            let schema = entry.status?.schema?;
            Some((entry.pid.to_string(), schema.eq_ignore_ascii_case("InStock")))
        })
        .collect()
}

pub(crate) fn parse_search_response(body: &str, limit: usize) -> Result<Vec<Product>> {
    let response: SearchResponse = serde_json::from_str(body)?;

    let brands: HashMap<&str, &str> = response
        .included
        .iter()
        .filter(|i| i.kind == "brands")
        .filter_map(|i| Some((i.id.as_str(), i.attributes.name.as_deref()?)))
        .collect();

    Ok(response
        .data
        .iter()
        .filter_map(|item| {
            let name = item.attributes.name.as_deref()?.trim();
            if name.is_empty() {
                return None;
            }
            let brand = item
                .relationships
                .as_ref()
                .and_then(|r| r.brand.as_ref())
                .and_then(|b| b.data.as_ref())
                .and_then(|d| brands.get(d.id.as_str()))
                .map(|b| b.to_string());

            Some(Product {
                id: item.id.clone(),
                name: name.to_string(),
                price_euro: item.attributes.price.as_ref().and_then(|p| p.current).unwrap_or(0.0),
                source: Source::Verkkokauppa,
                url: item
                    .attributes
                    .href
                    .as_deref()
                    .map(|href| crate::util::absolute_url(ORIGIN, href))
                    .unwrap_or_else(|| format!("{}/fi/product/{}", ORIGIN, item.id)),
                image_url: item.attributes.images.first().and_then(|i| i.orig.clone()),
                in_stock: None,
                ean: item
                    .attributes
                    .articles
                    .iter()
                    .flat_map(|a| a.eans.iter())
                    .find(|e| !e.trim().is_empty())
                    .cloned(),
                sku: None,
                brand,
                scraped_at: Utc::now(),
            })
        })
        .take(limit)
        .collect())
}

#[derive(Debug, Deserialize)]
struct WebApiProduct {
    pid: Option<serde_json::Value>,
    name: Option<Localized>,
    href: Option<Localized>,
    #[serde(default)]
    price: Option<Price>,
    #[serde(default)]
    eans: Vec<String>,
    #[serde(default)]
    mpns: Vec<String>,
    brand: Option<Brand>,
    #[serde(default)]
    images: Option<serde_json::Value>,
}

#[derive(Debug, Deserialize)]
struct Localized {
    fi: Option<String>,
    en: Option<String>,
}

impl Localized {
    fn preferred(&self) -> Option<String> {
        self.fi.clone().or_else(|| self.en.clone())
    }
}

#[derive(Debug, Deserialize)]
struct Brand {
    name: Option<String>,
}

/// Images arrive as an object keyed by pixel width; the largest key is the
/// closest thing to an original.
fn largest_image(value: &serde_json::Value) -> Option<String> {
    match value {
        serde_json::Value::Object(map) => map
            .iter()
            .filter_map(|(k, v)| Some((k.parse::<u32>().ok()?, v.as_str()?)))
            .max_by_key(|(width, _)| *width)
            .map(|(_, url)| url.to_string()),
        serde_json::Value::Array(items) => items.first().and_then(|i| i.as_str()).map(str::to_string),
        serde_json::Value::String(s) => Some(s.clone()),
        _ => None,
    }
}

pub(crate) fn parse_product_response(body: &str) -> Option<Product> {
    let products: Vec<WebApiProduct> = serde_json::from_str(body).ok()?;
    let item = products.into_iter().next()?;

    let id = match item.pid.as_ref()? {
        serde_json::Value::Number(n) => n.to_string(),
        serde_json::Value::String(s) => s.clone(),
        _ => return None,
    };
    let name = item.name.as_ref()?.preferred()?;

    Some(Product {
        id: id.clone(),
        name,
        price_euro: item.price.as_ref().and_then(|p| p.current).unwrap_or(0.0),
        source: Source::Verkkokauppa,
        url: item
            .href
            .as_ref()
            .and_then(|h| h.preferred())
            .map(|href| crate::util::absolute_url(ORIGIN, &href))
            .unwrap_or_else(|| format!("{}/fi/product/{}", ORIGIN, id)),
        image_url: item.images.as_ref().and_then(largest_image),
        in_stock: None,
        ean: item.eans.into_iter().find(|e| !e.trim().is_empty()),
        sku: item.mpns.into_iter().find(|m| !m.trim().is_empty()),
        brand: item.brand.and_then(|b| b.name),
        scraped_at: Utc::now(),
    })
}

impl VerkkokauppaSource {
    /// Search results carry no stock field, so availability is fetched
    /// separately and merged in.
    async fn attach_availability(&self, products: &mut [Product]) {
        for chunk in products.chunks_mut(AVAILABILITY_BATCH) {
            let pids = chunk
                .iter()
                .map(|p| p.id.as_str())
                .collect::<Vec<_>>()
                .join(",");
            let url = format!("{}?pids={}", AVAILABILITY_API, pids);

            let Ok(response) =
                crate::http::get_with_retry(&self.client, &url, self.policy, Some(ORIGIN)).await
            else {
                continue;
            };
            if !response.status().is_success() {
                continue;
            }
            let Ok(body) = response.text().await else {
                continue;
            };

            let availability = parse_availability(&body);
            for product in chunk.iter_mut() {
                product.in_stock = availability.get(&product.id).copied();
            }
        }
    }
}

#[async_trait]
impl RetailerSource for VerkkokauppaSource {
    fn source(&self) -> Source {
        Source::Verkkokauppa
    }

    async fn search(&self, query: &str, limit: usize) -> Result<Vec<Product>> {
        let page_size = limit.clamp(1, 100).to_string();
        // `filter[q]` is load-bearing: a plain `q` returns the unfiltered
        // catalogue with HTTP 200, which looks like success but is not.
        let response = self
            .client
            .get(SEARCH_API)
            .query(&[
                ("filter[q]", query),
                ("page[number]", "1"),
                ("page[size]", &page_size),
                ("sort", "-score"),
                ("sessionId", "hinta-cli"),
                ("include", "brand"),
            ])
            .send()
            .await?;

        let status = response.status();
        if !status.is_success() {
            bail!("verkkokauppa.com search returned HTTP {}", status.as_u16());
        }

        let mut products = parse_search_response(&response.text().await?, limit)?;
        self.attach_availability(&mut products).await;
        Ok(products)
    }

    async fn get_product(&self, product_id: &str) -> Result<Option<Product>> {
        let id = if product_id.starts_with("http") {
            Self::extract_id_from_url(product_id).ok_or_else(|| {
                anyhow::anyhow!("could not read a Verkkokauppa product id from {}", product_id)
            })?
        } else {
            product_id.to_string()
        };

        let url = format!("{}/{}", PRODUCT_API, id);
        let response =
            crate::http::get_with_retry(&self.client, &url, self.policy, Some(ORIGIN)).await?;
        let status = response.status();
        if matches!(status.as_u16(), 404 | 410) {
            return Ok(None);
        }
        if !status.is_success() {
            bail!("verkkokauppa.com product lookup returned HTTP {}", status.as_u16());
        }

        let Some(mut product) = parse_product_response(&response.text().await?) else {
            return Ok(None);
        };
        self.attach_availability(std::slice::from_mut(&mut product)).await;
        Ok(Some(product))
    }

    async fn get_category_products(&self, category_id: &str, limit: usize) -> Result<Vec<Product>> {
        self.search(category_id, limit).await
    }

    fn extract_id_from_url(url: &str) -> Option<String> {
        let marker = "/product/";
        let start = url.find(marker)? + marker.len();
        let id: String = url[start..]
            .chars()
            .take_while(|c| c.is_ascii_digit())
            .collect();
        (!id.is_empty()).then_some(id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SEARCH_JSON: &str = r#"{
      "data": [
        {"type":"products","id":"853450",
         "attributes":{
           "name":"AMD Ryzen 7 7800X3D -prosessori AM5 -kantaan",
           "href":"/fi/product/853450/AMD-Ryzen-7-7800X3D-prosessori",
           "price":{"current":365.99,"currentTaxless":291.63},
           "images":[{"orig":"https://cdn.verk.net/images/40/2_853450-823x774.jpg"}],
           "articles":[{"articleId":133048,"eans":["0730143314930"],"pid":853450}]},
         "relationships":{"brand":{"data":{"id":"171","type":"brands"}}}},
        {"type":"products","id":"999",
         "attributes":{"name":"Nimetön","href":"/fi/product/999/x","price":{"current":null},
                       "images":[],"articles":[]}}
      ],
      "included":[{"type":"brands","id":"171","attributes":{"name":"AMD","slug":"amd"}}],
      "meta":{"totalResults":4}}"#;

    #[test]
    fn parses_the_json_api_search_payload() {
        let products = parse_search_response(SEARCH_JSON, 10).unwrap();
        assert_eq!(products.len(), 2);

        let cpu = &products[0];
        assert_eq!(cpu.id, "853450");
        assert_eq!(cpu.name, "AMD Ryzen 7 7800X3D -prosessori AM5 -kantaan");
        assert_eq!(cpu.price_euro, 365.99);
        assert_eq!(cpu.ean.as_deref(), Some("0730143314930"));
        assert_eq!(
            cpu.url,
            "https://www.verkkokauppa.com/fi/product/853450/AMD-Ryzen-7-7800X3D-prosessori"
        );
        assert_eq!(
            cpu.image_url.as_deref(),
            Some("https://cdn.verk.net/images/40/2_853450-823x774.jpg")
        );
    }

    #[test]
    fn resolves_the_brand_through_the_included_section() {
        let products = parse_search_response(SEARCH_JSON, 10).unwrap();
        assert_eq!(products[0].brand.as_deref(), Some("AMD"));
        assert_eq!(products[1].brand, None);
    }

    #[test]
    fn a_null_price_becomes_zero_rather_than_failing() {
        let products = parse_search_response(SEARCH_JSON, 10).unwrap();
        assert_eq!(products[1].price_euro, 0.0);
    }

    #[test]
    fn search_leaves_stock_unknown_until_availability_is_merged() {
        let products = parse_search_response(SEARCH_JSON, 10).unwrap();
        assert_eq!(products[0].in_stock, None);
    }

    #[test]
    fn limit_truncates_the_result_set() {
        assert_eq!(parse_search_response(SEARCH_JSON, 1).unwrap().len(), 1);
    }

    #[test]
    fn parses_the_availability_payload() {
        let body = r#"[
          {"pid":853450,"status":{"og":"in stock","schema":"InStock"}},
          {"pid":999,"status":{"og":"sold out","schema":"OutOfStock"}},
          {"pid":1000}
        ]"#;
        let map = parse_availability(body);
        assert_eq!(map.get("853450"), Some(&true));
        assert_eq!(map.get("999"), Some(&false));
        assert_eq!(map.get("1000"), None);
    }

    #[test]
    fn malformed_availability_degrades_to_unknown_rather_than_failing() {
        assert!(parse_availability("not json").is_empty());
        assert!(parse_availability("[]").is_empty());
    }

    #[test]
    fn parses_the_single_product_payload() {
        let body = r#"[{
          "pid":853450,
          "name":{"fi":"AMD Ryzen 7 7800X3D -prosessori","en":"AMD Ryzen 7 7800X3D processor"},
          "href":{"fi":"/fi/product/853450/amd-ryzen"},
          "price":{"current":365.99},
          "eans":["0730143314930"],
          "mpns":["100-100000910WOF"],
          "brand":{"id":"171","name":"AMD"},
          "images":{"45":"https://cdn.verk.net/s.jpg","800":"https://cdn.verk.net/l.jpg"}
        }]"#;

        let product = parse_product_response(body).unwrap();
        assert_eq!(product.id, "853450");
        assert_eq!(product.name, "AMD Ryzen 7 7800X3D -prosessori");
        assert_eq!(product.price_euro, 365.99);
        assert_eq!(product.ean.as_deref(), Some("0730143314930"));
        assert_eq!(product.sku.as_deref(), Some("100-100000910WOF"));
        assert_eq!(product.brand.as_deref(), Some("AMD"));
        assert_eq!(product.image_url.as_deref(), Some("https://cdn.verk.net/l.jpg"));
    }

    #[test]
    fn an_empty_product_array_is_not_found() {
        assert!(parse_product_response("[]").is_none());
        assert!(parse_product_response("garbage").is_none());
    }

    #[test]
    fn extracts_the_id_from_a_product_url() {
        assert_eq!(
            VerkkokauppaSource::extract_id_from_url(
                "https://www.verkkokauppa.com/fi/product/853450/AMD-Ryzen"
            ),
            Some("853450".to_string())
        );
        assert_eq!(
            VerkkokauppaSource::extract_id_from_url("https://www.verkkokauppa.com/fi/etusivu"),
            None
        );
    }
}
