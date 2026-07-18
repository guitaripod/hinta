use anyhow::{bail, Result};
use async_trait::async_trait;
use chrono::Utc;
use serde::Deserialize;

use crate::http::{RetryPolicy, CHROME_UA};
use crate::transform::types::{Product, Source};

use super::RetailerSource;

const ORIGIN: &str = "https://www.power.fi";
const SEARCH_API: &str = "https://www.power.fi/api/v2/productlists";
const PRODUCT_API: &str = "https://www.power.fi/api/v2/products";
const MEDIA_ORIGIN: &str = "https://media.power-cdn.net";

#[derive(Debug)]
pub struct PowerSource {
    client: reqwest::Client,
    policy: RetryPolicy,
}

impl Default for PowerSource {
    fn default() -> Self {
        Self::new()
    }
}

impl PowerSource {
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
    products: Vec<ApiProduct>,
}

#[derive(Debug, Deserialize)]
struct ApiProduct {
    #[serde(rename = "productId")]
    product_id: Option<i64>,
    title: Option<String>,
    url: Option<String>,
    price: Option<f64>,
    #[serde(rename = "manufacturerName")]
    manufacturer_name: Option<String>,
    #[serde(rename = "eanGtin12")]
    ean_gtin12: Option<String>,
    barcode: Option<String>,
    #[serde(rename = "productManufactorIdentity")]
    manufacturer_identity: Option<String>,
    #[serde(rename = "webStockMeta")]
    web_stock_meta: Option<String>,
    #[serde(rename = "productImage")]
    product_image: Option<ProductImage>,
}

#[derive(Debug, Deserialize)]
struct ProductImage {
    #[serde(rename = "basePath")]
    base_path: Option<String>,
    #[serde(default)]
    variants: Vec<ImageVariant>,
}

#[derive(Debug, Deserialize)]
struct ImageVariant {
    filename: Option<String>,
    #[serde(default)]
    width: Option<u32>,
}

impl ProductImage {
    /// Picks the widest variant so downstream consumers get a usable image.
    fn best_url(&self) -> Option<String> {
        let base = self.base_path.as_deref()?.trim_end_matches('/');
        let filename = self
            .variants
            .iter()
            .filter(|v| v.filename.is_some())
            .max_by_key(|v| v.width.unwrap_or(0))
            .and_then(|v| v.filename.as_deref())?;
        Some(format!("{}{}/{}", MEDIA_ORIGIN, base, filename))
    }
}

impl ApiProduct {
    fn into_product(self) -> Option<Product> {
        let id = self.product_id?.to_string();
        let name = self.title?.trim().to_string();
        if name.is_empty() {
            return None;
        }

        Some(Product {
            id: id.clone(),
            name,
            price_euro: self.price.unwrap_or(0.0),
            source: Source::Power,
            url: self
                .url
                .as_deref()
                .map(|u| crate::util::absolute_url(ORIGIN, u))
                .unwrap_or_else(|| format!("{}/p-{}/", ORIGIN, id)),
            image_url: self.product_image.as_ref().and_then(ProductImage::best_url),
            in_stock: self
                .web_stock_meta
                .as_deref()
                .map(|meta| meta.eq_ignore_ascii_case("InStock")),
            ean: self
                .ean_gtin12
                .or(self.barcode)
                .filter(|e| !e.trim().is_empty()),
            sku: self.manufacturer_identity.filter(|s| !s.trim().is_empty()),
            brand: self.manufacturer_name.filter(|b| !b.trim().is_empty()),
            scraped_at: Utc::now(),
        })
    }
}

pub(crate) fn parse_search_response(body: &str, limit: usize) -> Result<Vec<Product>> {
    let response: SearchResponse = serde_json::from_str(body)?;
    Ok(response
        .products
        .into_iter()
        .filter_map(ApiProduct::into_product)
        .take(limit)
        .collect())
}

pub(crate) fn parse_product_response(body: &str) -> Option<Product> {
    let products: Vec<ApiProduct> = serde_json::from_str(body).ok()?;
    products.into_iter().find_map(ApiProduct::into_product)
}

#[async_trait]
impl RetailerSource for PowerSource {
    fn source(&self) -> Source {
        Source::Power
    }

    async fn search(&self, query: &str, limit: usize) -> Result<Vec<Product>> {
        let size = limit.clamp(1, 100).to_string();
        let response = self
            .client
            .get(SEARCH_API)
            .query(&[("q", query), ("size", &size), ("from", "0")])
            .send()
            .await?;

        let status = response.status();
        if !status.is_success() {
            bail!("power.fi search returned HTTP {}", status.as_u16());
        }
        parse_search_response(&response.text().await?, limit)
    }

    async fn get_product(&self, product_id: &str) -> Result<Option<Product>> {
        let id = if product_id.starts_with("http") {
            Self::extract_id_from_url(product_id).ok_or_else(|| {
                anyhow::anyhow!("could not read a Power product id from {}", product_id)
            })?
        } else {
            product_id.to_string()
        };

        let url = format!("{}?ids={}", PRODUCT_API, id);
        let response =
            crate::http::get_with_retry(&self.client, &url, self.policy, Some(ORIGIN)).await?;
        let status = response.status();
        if matches!(status.as_u16(), 404 | 410) {
            return Ok(None);
        }
        if !status.is_success() {
            bail!("power.fi product lookup returned HTTP {}", status.as_u16());
        }
        Ok(parse_product_response(&response.text().await?))
    }

    async fn get_category_products(&self, category_id: &str, limit: usize) -> Result<Vec<Product>> {
        self.search(category_id, limit).await
    }

    fn extract_id_from_url(url: &str) -> Option<String> {
        let marker = "/p-";
        let start = url.rfind(marker)? + marker.len();
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
      "totalProductCount": 3, "isLastPage": true,
      "products": [
        {"productId":4280074,
         "title":"Cepter Extreme R97 Wi-Fi (16/1TB/7800X3D) -pöytäkone",
         "url":"/pelaaminen/pc/cepter-extreme-r97/p-4280074/",
         "price":2099.0,"vatlessPrice":1672.51,
         "manufacturerName":"Cepter","barcode":"5744003415557","eanGtin12":"5744003415557",
         "stockCount":93,"webStockStatus":1,"webStockMeta":"InStock",
         "productManufactorIdentity":"CEXR797XTBKB",
         "productImage":{"basePath":"/images/h-2fce/products/4280074",
           "variants":[{"filename":"a_300x300.webp","width":300},
                       {"filename":"a_600x600.webp","width":600}]}},
        {"productId":4280075,"title":"Sold out thing","url":"/x/p-4280075/",
         "price":19.9,"webStockMeta":"OutOfStock","manufacturerName":"Acme",
         "productImage":{"basePath":"/images/x","variants":[]}}
      ]}"#;

    #[test]
    fn parses_the_search_payload() {
        let products = parse_search_response(SEARCH_JSON, 10).unwrap();
        assert_eq!(products.len(), 2);

        let pc = &products[0];
        assert_eq!(pc.id, "4280074");
        assert_eq!(pc.name, "Cepter Extreme R97 Wi-Fi (16/1TB/7800X3D) -pöytäkone");
        assert_eq!(pc.price_euro, 2099.0);
        assert_eq!(pc.ean.as_deref(), Some("5744003415557"));
        assert_eq!(pc.sku.as_deref(), Some("CEXR797XTBKB"));
        assert_eq!(pc.brand.as_deref(), Some("Cepter"));
        assert_eq!(pc.in_stock, Some(true));
        assert_eq!(
            pc.url,
            "https://www.power.fi/pelaaminen/pc/cepter-extreme-r97/p-4280074/"
        );
    }

    #[test]
    fn picks_the_widest_image_variant() {
        let products = parse_search_response(SEARCH_JSON, 10).unwrap();
        assert_eq!(
            products[0].image_url.as_deref(),
            Some("https://media.power-cdn.net/images/h-2fce/products/4280074/a_600x600.webp")
        );
    }

    #[test]
    fn a_variantless_image_yields_no_url() {
        let products = parse_search_response(SEARCH_JSON, 10).unwrap();
        assert_eq!(products[1].image_url, None);
    }

    #[test]
    fn reads_the_stock_meta() {
        let products = parse_search_response(SEARCH_JSON, 10).unwrap();
        assert_eq!(products[0].in_stock, Some(true));
        assert_eq!(products[1].in_stock, Some(false));
    }

    #[test]
    fn limit_truncates_the_result_set() {
        assert_eq!(parse_search_response(SEARCH_JSON, 1).unwrap().len(), 1);
    }

    #[test]
    fn an_empty_catalogue_response_is_not_an_error() {
        let products = parse_search_response(r#"{"products":[],"totalProductCount":0}"#, 10).unwrap();
        assert!(products.is_empty());
    }

    #[test]
    fn parses_the_single_product_array() {
        let body = r#"[{"productId":4280074,"title":"Thing","url":"/x/p-4280074/",
                        "price":10.0,"webStockMeta":"InStock"}]"#;
        let product = parse_product_response(body).unwrap();
        assert_eq!(product.id, "4280074");
        assert_eq!(product.price_euro, 10.0);
    }

    #[test]
    fn an_empty_product_array_is_not_found() {
        assert!(parse_product_response("[]").is_none());
        assert!(parse_product_response("nonsense").is_none());
    }

    #[test]
    fn extracts_the_id_from_a_product_url() {
        assert_eq!(
            PowerSource::extract_id_from_url(
                "https://www.power.fi/pelaaminen/pc/cepter-extreme/p-4280074/"
            ),
            Some("4280074".to_string())
        );
        assert_eq!(PowerSource::extract_id_from_url("https://www.power.fi/"), None);
    }
}
