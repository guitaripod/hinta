use anyhow::{bail, Result};
use async_trait::async_trait;
use chrono::Utc;
use scraper::{Html, Selector};
use serde::Deserialize;
use serde_json::json;

use crate::http::{RetryPolicy, CHROME_UA};
use crate::transform::types::{Product, Source};

use super::RetailerSource;

const ORIGIN: &str = "https://www.jimms.fi";
const SEARCH_API: &str = "https://www.jimms.fi/api/product/newbetasearch";

#[derive(Debug)]
pub struct JimmsSource {
    client: reqwest::Client,
    policy: RetryPolicy,
}

impl Default for JimmsSource {
    fn default() -> Self {
        Self::new()
    }
}

impl JimmsSource {
    /// Jimms answers a request with no User-Agent with 403, so one is always sent.
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
    #[serde(rename = "Products", default)]
    products: Vec<ApiProduct>,
    #[serde(rename = "IsError", default)]
    is_error: bool,
    #[serde(rename = "ErrorMessage")]
    error_message: Option<String>,
}

#[derive(Debug, Deserialize)]
struct ApiProduct {
    #[serde(rename = "ProductID")]
    product_id: Option<i64>,
    #[serde(rename = "Code")]
    code: Option<String>,
    #[serde(rename = "VendorName")]
    vendor_name: Option<String>,
    #[serde(rename = "Name")]
    name: Option<String>,
    #[serde(rename = "PriceTax")]
    price_tax: Option<f64>,
    #[serde(rename = "Uri")]
    uri: Option<String>,
    #[serde(rename = "ImageID")]
    image_id: Option<i64>,
    #[serde(rename = "ImageBaseSrc")]
    image_base_src: Option<String>,
    #[serde(rename = "ItemAvailability")]
    item_availability: Option<i32>,
}

/// Maps Jimms' availability enum onto "can I have it now".
///
/// `4` means orderable from the supplier rather than held in stock, so it is
/// reported as out of stock — a comparison that claims otherwise misleads.
fn availability_to_stock(code: Option<i32>) -> Option<bool> {
    match code {
        Some(1) => Some(true),
        Some(0) | Some(2) | Some(4) => Some(false),
        _ => None,
    }
}

impl ApiProduct {
    fn into_product(self) -> Option<Product> {
        let id = self.product_id?.to_string();
        let raw_name = self.name?.trim().to_string();
        if raw_name.is_empty() {
            return None;
        }

        let vendor = self.vendor_name.as_deref().map(str::trim).filter(|v| !v.is_empty());
        let name = match vendor {
            Some(vendor)
                if !raw_name
                    .to_lowercase()
                    .starts_with(&vendor.to_lowercase()) =>
            {
                format!("{} {}", vendor, raw_name)
            }
            _ => raw_name,
        };

        let url = match self.uri.as_deref().map(str::trim).filter(|u| !u.is_empty()) {
            Some(uri) => format!("{}/fi/{}", ORIGIN, uri.trim_start_matches('/')),
            None => format!("{}/fi/Product/Show/{}", ORIGIN, id),
        };

        let image_url = match (self.image_base_src.as_deref(), self.image_id) {
            (Some(base), Some(image_id)) if !base.trim().is_empty() => Some(
                crate::util::absolute_url(ORIGIN, &format!("{}{}-ig400gg.jpg", base, image_id)),
            ),
            _ => None,
        };

        Some(Product {
            id,
            name,
            price_euro: self.price_tax.unwrap_or(0.0),
            source: Source::Jimms,
            url,
            image_url,
            in_stock: availability_to_stock(self.item_availability),
            ean: None,
            sku: self.code.filter(|c| !c.trim().is_empty()),
            brand: vendor.map(|v| v.to_string()),
            scraped_at: Utc::now(),
        })
    }
}

pub(crate) fn parse_search_response(body: &str, limit: usize) -> Result<Vec<Product>> {
    let response: SearchResponse = serde_json::from_str(body)?;
    if response.is_error {
        bail!(
            "jimms.fi search failed: {}",
            response
                .error_message
                .unwrap_or_else(|| "unknown error".into())
        );
    }
    Ok(response
        .products
        .into_iter()
        .filter_map(ApiProduct::into_product)
        .take(limit)
        .collect())
}

/// Reads a microdata property, preferring an explicit `content` attribute over
/// the rendered text.
fn microdata(doc: &Html, prop: &str) -> Option<String> {
    let selector = Selector::parse(&format!("[itemprop=\"{}\"]", prop)).ok()?;
    let element = doc.select(&selector).next()?;
    let value = element
        .value()
        .attr("content")
        .map(|c| c.to_string())
        .unwrap_or_else(|| element.text().collect::<String>());
    let trimmed = crate::util::squeeze_whitespace(&value);
    (!trimmed.is_empty()).then_some(trimmed)
}

pub(crate) fn parse_product_page(html: &str, product_id: &str) -> Option<Product> {
    let doc = Html::parse_document(html);

    let name = microdata(&doc, "name").or_else(|| {
        let selector = Selector::parse("h1").ok()?;
        doc.select(&selector)
            .next()
            .map(|el| crate::util::squeeze_whitespace(&el.text().collect::<String>()))
            .filter(|n| !n.is_empty())
    })?;

    let price = microdata(&doc, "price")
        .as_deref()
        .and_then(crate::util::parse_price)
        .unwrap_or(0.0);

    let in_stock = microdata(&doc, "availability").map(|a| {
        let a = a.to_lowercase();
        a.contains("instock") || a.contains("limitedavailability")
    });

    Some(Product {
        id: product_id.to_string(),
        name,
        price_euro: price,
        source: Source::Jimms,
        url: format!("{}/fi/Product/Show/{}", ORIGIN, product_id),
        image_url: microdata(&doc, "image").map(|src| crate::util::absolute_url(ORIGIN, &src)),
        in_stock,
        ean: microdata(&doc, "gtin13").or_else(|| microdata(&doc, "gtin")),
        sku: microdata(&doc, "mpn"),
        brand: microdata(&doc, "brand"),
        scraped_at: Utc::now(),
    })
}

#[async_trait]
impl RetailerSource for JimmsSource {
    fn source(&self) -> Source {
        Source::Jimms
    }

    async fn search(&self, query: &str, limit: usize) -> Result<Vec<Product>> {
        let payload = json!({
            "SearchQuery": query,
            "Page": 1,
            "Items": limit.clamp(1, 100),
        });

        let response = self.client.post(SEARCH_API).json(&payload).send().await?;
        let status = response.status();
        if !status.is_success() {
            bail!("jimms.fi search returned HTTP {}", status.as_u16());
        }
        parse_search_response(&response.text().await?, limit)
    }

    async fn get_product(&self, product_id: &str) -> Result<Option<Product>> {
        let (url, id) = if product_id.starts_with("http") {
            let id = Self::extract_id_from_url(product_id).ok_or_else(|| {
                anyhow::anyhow!("could not read a Jimms product id from {}", product_id)
            })?;
            (product_id.to_string(), id)
        } else {
            (
                format!("{}/fi/Product/Show/{}", ORIGIN, product_id),
                product_id.to_string(),
            )
        };

        let response =
            crate::http::get_with_retry(&self.client, &url, self.policy, Some(ORIGIN)).await?;
        let status = response.status();
        // A delisted product answers 410 Gone rather than 404.
        if matches!(status.as_u16(), 404 | 410) {
            return Ok(None);
        }
        if !status.is_success() {
            bail!("jimms.fi product page returned HTTP {}", status.as_u16());
        }
        Ok(parse_product_page(&response.text().await?, &id))
    }

    async fn get_category_products(&self, category_id: &str, limit: usize) -> Result<Vec<Product>> {
        self.search(category_id, limit).await
    }

    fn extract_id_from_url(url: &str) -> Option<String> {
        let lowered = url.to_lowercase();
        let marker = "/product/show/";
        let start = lowered.find(marker)? + marker.len();
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
      "Count": 2, "IsError": false,
      "Products": [
        {"ProductID": 188803, "Code": "100-100000910WOF", "VendorName": "AMD",
         "Name": "Ryzen 7 7800X3D, AM5, 4.2 GHz, 8-Core, WOF",
         "Price": 290.76, "PriceTax": 364.9,
         "Uri": "Product/Show/188803/100-100000910wof/amd-ryzen-7-7800x3d",
         "ImageID": 428633, "ImageBaseSrc": "//ic.jimms.fi/product/3/6/",
         "DeliveryInfoText": "Varastossa 13 kpl", "ItemAvailability": 1},
        {"ProductID": 200001, "Code": "BX8071514600K", "VendorName": "Intel",
         "Name": "Core i5-14600K", "PriceTax": 319.0,
         "Uri": "Product/Show/200001/x/intel-core-i5-14600k",
         "ImageID": null, "ImageBaseSrc": "//ic.jimms.fi/product/1/1/",
         "ItemAvailability": 4}
      ]}"#;

    #[test]
    fn parses_the_search_payload() {
        let products = parse_search_response(SEARCH_JSON, 10).unwrap();
        assert_eq!(products.len(), 2);

        let cpu = &products[0];
        assert_eq!(cpu.id, "188803");
        assert_eq!(cpu.name, "AMD Ryzen 7 7800X3D, AM5, 4.2 GHz, 8-Core, WOF");
        assert_eq!(cpu.price_euro, 364.9);
        assert_eq!(cpu.sku.as_deref(), Some("100-100000910WOF"));
        assert_eq!(cpu.brand.as_deref(), Some("AMD"));
        assert_eq!(cpu.in_stock, Some(true));
        assert_eq!(
            cpu.url,
            "https://www.jimms.fi/fi/Product/Show/188803/100-100000910wof/amd-ryzen-7-7800x3d"
        );
        assert_eq!(
            cpu.image_url.as_deref(),
            Some("https://ic.jimms.fi/product/3/6/428633-ig400gg.jpg")
        );
    }

    #[test]
    fn uses_the_vat_inclusive_price() {
        let products = parse_search_response(SEARCH_JSON, 10).unwrap();
        assert_eq!(products[0].price_euro, 364.9);
    }

    #[test]
    fn orderable_from_supplier_is_not_in_stock() {
        let products = parse_search_response(SEARCH_JSON, 10).unwrap();
        assert_eq!(products[1].in_stock, Some(false));
        assert_eq!(availability_to_stock(Some(1)), Some(true));
        assert_eq!(availability_to_stock(Some(0)), Some(false));
        assert_eq!(availability_to_stock(None), None);
    }

    #[test]
    fn a_missing_image_id_yields_no_image() {
        let products = parse_search_response(SEARCH_JSON, 10).unwrap();
        assert_eq!(products[1].image_url, None);
    }

    #[test]
    fn brand_is_not_prepended_twice() {
        let json =
            r#"{"Products":[{"ProductID":1,"VendorName":"AMD","Name":"AMD Ryzen 5","PriceTax":100.0}]}"#;
        let products = parse_search_response(json, 10).unwrap();
        assert_eq!(products[0].name, "AMD Ryzen 5");
    }

    #[test]
    fn limit_is_honoured_client_side() {
        assert_eq!(parse_search_response(SEARCH_JSON, 1).unwrap().len(), 1);
    }

    #[test]
    fn an_error_payload_surfaces_the_message() {
        let json = r#"{"IsError":true,"ErrorMessage":"boom","Products":[]}"#;
        let err = parse_search_response(json, 10).unwrap_err().to_string();
        assert!(err.contains("boom"), "unexpected error: {}", err);
    }

    #[test]
    fn an_empty_result_set_is_not_an_error() {
        let products = parse_search_response(r#"{"Count":0,"Products":[]}"#, 10).unwrap();
        assert!(products.is_empty());
    }

    #[test]
    fn a_product_missing_its_id_is_skipped_rather_than_fatal() {
        let json = r#"{"Products":[{"Name":"Nameless","PriceTax":1.0},
                                   {"ProductID":5,"Name":"Fine","PriceTax":2.0}]}"#;
        let products = parse_search_response(json, 10).unwrap();
        assert_eq!(products.len(), 1);
        assert_eq!(products[0].id, "5");
    }

    #[test]
    fn parses_the_product_page_microdata_for_the_ean() {
        let html = r#"<html><body>
          <h1 itemprop="name">AMD Ryzen 7 7800X3D</h1>
          <span itemprop="gtin13">0730143314930</span>
          <span itemprop="mpn">100-100000910WOF</span>
          <span itemprop="brand">AMD</span>
          <meta itemprop="price" content="364.90">
          <link itemprop="availability" content="https://schema.org/InStock">
          <img itemprop="image" content="//ic.jimms.fi/a.jpg">
        </body></html>"#;

        let product = parse_product_page(html, "188803").unwrap();
        assert_eq!(product.name, "AMD Ryzen 7 7800X3D");
        assert_eq!(product.ean.as_deref(), Some("0730143314930"));
        assert_eq!(product.sku.as_deref(), Some("100-100000910WOF"));
        assert_eq!(product.price_euro, 364.90);
        assert_eq!(product.in_stock, Some(true));
        assert_eq!(product.image_url.as_deref(), Some("https://ic.jimms.fi/a.jpg"));
    }

    #[test]
    fn falls_back_to_the_h1_when_microdata_is_absent() {
        let html = "<html><body><h1>  Plain   Product  </h1></body></html>";
        let product = parse_product_page(html, "1").unwrap();
        assert_eq!(product.name, "Plain Product");
        assert_eq!(product.price_euro, 0.0);
        assert_eq!(product.ean, None);
    }

    #[test]
    fn extracts_the_id_from_a_product_url() {
        assert_eq!(
            JimmsSource::extract_id_from_url(
                "https://www.jimms.fi/fi/Product/Show/188803/code/slug-here"
            ),
            Some("188803".to_string())
        );
        assert_eq!(
            JimmsSource::extract_id_from_url("https://www.jimms.fi/fi/product/show/42"),
            Some("42".to_string())
        );
        assert_eq!(
            JimmsSource::extract_id_from_url("https://www.jimms.fi/fi/"),
            None
        );
    }
}
