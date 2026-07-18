use anyhow::{bail, Result};
use async_trait::async_trait;
use chrono::Utc;
use scraper::{ElementRef, Html, Selector};

use crate::http::{RetryPolicy, CHROME_UA};
use crate::transform::types::{Product, Source};

use super::{jsonld, RetailerSource};

const ORIGIN: &str = "https://www.multitronic.fi";
/// `PageSpeed=off` stops mod_pagespeed rewriting image URLs into hashed variants.
const SEARCH_ENDPOINT: &str = "https://www.multitronic.fi/fi/search/gpl?PageSpeed=off";

#[derive(Debug)]
pub struct MultitronicSource {
    client: reqwest::Client,
    policy: RetryPolicy,
}

impl Default for MultitronicSource {
    fn default() -> Self {
        Self::new()
    }
}

impl MultitronicSource {
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

struct CardSelectors {
    card: Selector,
    data: Selector,
    title: Selector,
    image: Selector,
    stock: Selector,
}

impl CardSelectors {
    fn new() -> Self {
        Self {
            card: Selector::parse("div.item_wrapper").unwrap(),
            data: Selector::parse(".productDataEntry").unwrap(),
            title: Selector::parse("a.pTitle").unwrap(),
            image: Selector::parse(".product_image img").unwrap(),
            stock: Selector::parse(".product_stock .greytext").unwrap(),
        }
    }
}

/// Pulls the numeric product id out of `/fi/products/{id}/{slug}`.
fn id_from_path(href: &str) -> Option<String> {
    let marker = "/products/";
    let start = href.find(marker)? + marker.len();
    let id: String = href[start..]
        .chars()
        .take_while(|c| c.is_ascii_digit())
        .collect();
    (!id.is_empty()).then_some(id)
}

fn parse_card(card: &ElementRef, selectors: &CardSelectors) -> Option<Product> {
    let title = card.select(&selectors.title).next()?;
    let href = title.value().attr("href")?;
    let id = id_from_path(href)?;
    let name = crate::util::squeeze_whitespace(&title.text().collect::<String>());
    if name.is_empty() {
        return None;
    }

    let data = card.select(&selectors.data).next();
    // The hidden data node carries a machine-readable `359.90`; the visible
    // price element concatenates the discounted and struck-through prices.
    let price = data
        .and_then(|d| d.value().attr("data-price").and_then(crate::util::parse_price))?;

    let sku = data
        .and_then(|d| d.value().attr("data-code"))
        .or_else(|| card.value().attr("data-code"))
        .map(str::to_string)
        .filter(|s| !s.trim().is_empty());

    let brand = data
        .and_then(|d| d.value().attr("data-brand"))
        .map(str::to_string)
        .filter(|b| !b.trim().is_empty());

    let image_url = card
        .select(&selectors.image)
        .next()
        .and_then(|img| img.value().attr("src"))
        .map(|src| crate::util::absolute_url(ORIGIN, src));

    let in_stock = card
        .select(&selectors.stock)
        .next()
        .map(|el| el.text().collect::<String>())
        .and_then(|text| crate::util::parse_stock_phrase(&text));

    Some(Product {
        id,
        name,
        price_euro: price,
        source: Source::Multitronic,
        url: crate::util::absolute_url(ORIGIN, href),
        image_url,
        in_stock,
        ean: None,
        sku,
        brand,
        scraped_at: Utc::now(),
    })
}

pub(crate) fn parse_search_fragment(html: &str, limit: usize) -> Vec<Product> {
    let doc = Html::parse_fragment(html);
    let selectors = CardSelectors::new();
    doc.select(&selectors.card)
        .filter_map(|card| parse_card(&card, &selectors))
        .take(limit)
        .collect()
}

pub(crate) fn parse_product_page(html: &str, product_id: &str) -> Option<Product> {
    let ld = jsonld::find_product(html)?;
    let name = ld.name?;

    Some(Product {
        id: product_id.to_string(),
        name,
        price_euro: ld.price_euro.unwrap_or(0.0),
        source: Source::Multitronic,
        url: ld
            .url
            .unwrap_or_else(|| format!("{}/fi/products/{}", ORIGIN, product_id)),
        image_url: ld.image.map(|src| crate::util::absolute_url(ORIGIN, &src)),
        in_stock: ld.in_stock,
        ean: ld.gtin,
        sku: ld.mpn,
        brand: ld.brand,
        scraped_at: Utc::now(),
    })
}

#[async_trait]
impl RetailerSource for MultitronicSource {
    fn source(&self) -> Source {
        Source::Multitronic
    }

    async fn search(&self, query: &str, limit: usize) -> Result<Vec<Product>> {
        // The server caps page size at 24 regardless of what is requested.
        let per_page = limit.clamp(1, 24);
        let response = self
            .client
            .post(SEARCH_ENDPOINT)
            .form(&[
                ("keywords", query),
                ("page", "1"),
                ("ppp", &per_page.to_string()),
            ])
            .send()
            .await?;

        let status = response.status();
        if !status.is_success() {
            bail!("multitronic.fi search returned HTTP {}", status.as_u16());
        }
        Ok(parse_search_fragment(&response.text().await?, limit))
    }

    async fn get_product(&self, product_id: &str) -> Result<Option<Product>> {
        let (url, id) = if product_id.starts_with("http") {
            let id = Self::extract_id_from_url(product_id).ok_or_else(|| {
                anyhow::anyhow!("could not read a Multitronic product id from {}", product_id)
            })?;
            (product_id.to_string(), id)
        } else {
            (
                format!("{}/fi/products/{}", ORIGIN, product_id),
                product_id.to_string(),
            )
        };

        let response =
            crate::http::get_with_retry(&self.client, &url, self.policy, Some(ORIGIN)).await?;
        let status = response.status();
        if matches!(status.as_u16(), 404 | 410) {
            return Ok(None);
        }
        if !status.is_success() {
            bail!("multitronic.fi product page returned HTTP {}", status.as_u16());
        }
        Ok(parse_product_page(&response.text().await?, &id))
    }

    async fn get_category_products(&self, category_id: &str, limit: usize) -> Result<Vec<Product>> {
        self.search(category_id, limit).await
    }

    fn extract_id_from_url(url: &str) -> Option<String> {
        id_from_path(url)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const CARD: &str = r#"
    <div class="item_wrapper listGridV3" data-code="100-100000910WOF">
      <div class="productDataEntry" style="display: none;" data-code="100-100000910WOF"
           data-name="AMD Ryzen 7 7800X3D 4.2 GHz 104 MB, AM5 - processor (WOF)"
           data-currency="EUR" data-price="359.90" data-brand="AMD"></div>
      <div class="product_image swiper" data-url="/fi/products/3930054/amd-ryzen-7-7800x3d">
        <img src="https://cdn.multitronic.fi/images/prod/2/E/100-100000910WOF-1.webp" alt="x">
      </div>
      <a class="pTitle pcTracker" href="/fi/products/3930054/amd-ryzen-7-7800x3d">AMD Ryzen 7 7800X3D 4.2 GHz 104 MB, AM5 -suoritin (WOF)</a>
      <div class="product_price ">359,90€ </div>
      <div class="listBottomWrapper">
        <div class="product_stock fsmall">
          <div class="semi-bold">Arvioitu toimitus: 29.07. - 03.08.</div>
          <div class="greytext">Varastossa</div>
        </div>
      </div>
    </div>"#;

    #[test]
    fn parses_a_search_card() {
        let products = parse_search_fragment(CARD, 10);
        assert_eq!(products.len(), 1);

        let p = &products[0];
        assert_eq!(p.id, "3930054");
        assert_eq!(p.name, "AMD Ryzen 7 7800X3D 4.2 GHz 104 MB, AM5 -suoritin (WOF)");
        assert_eq!(p.price_euro, 359.90);
        assert_eq!(p.sku.as_deref(), Some("100-100000910WOF"));
        assert_eq!(p.brand.as_deref(), Some("AMD"));
        assert_eq!(p.in_stock, Some(true));
        assert_eq!(
            p.url,
            "https://www.multitronic.fi/fi/products/3930054/amd-ryzen-7-7800x3d"
        );
        assert_eq!(
            p.image_url.as_deref(),
            Some("https://cdn.multitronic.fi/images/prod/2/E/100-100000910WOF-1.webp")
        );
    }

    #[test]
    fn reads_the_discounted_price_not_the_struck_through_one() {
        let html = r#"
        <div class="item_wrapper">
          <div class="productDataEntry" data-price="399.00" data-code="X1"></div>
          <a class="pTitle" href="/fi/products/1/x">Thing</a>
          <div class="product_price has_old_price">399,00€
            <span class="strike-center old_price">449,00€</span>
          </div>
        </div>"#;
        let products = parse_search_fragment(html, 10);
        assert_eq!(products[0].price_euro, 399.00);
    }

    #[test]
    fn reads_supplier_availability_as_out_of_stock() {
        let html = CARD.replace("Varastossa", "Saatavilla toimittajalta");
        let products = parse_search_fragment(&html, 10);
        assert_eq!(products[0].in_stock, Some(false));
    }

    #[test]
    fn a_card_without_a_machine_readable_price_is_skipped() {
        let html = r#"
        <div class="item_wrapper">
          <a class="pTitle" href="/fi/products/9/x">No price</a>
        </div>"#;
        assert!(parse_search_fragment(html, 10).is_empty());
    }

    #[test]
    fn limit_truncates_the_card_list() {
        let html = format!("{}{}", CARD, CARD.replace("3930054", "3930055"));
        assert_eq!(parse_search_fragment(&html, 1).len(), 1);
        assert_eq!(parse_search_fragment(&html, 5).len(), 2);
    }

    #[test]
    fn an_empty_result_fragment_yields_nothing() {
        assert!(parse_search_fragment("<div>Ei tuloksia</div>", 10).is_empty());
    }

    #[test]
    fn parses_the_detail_page_json_ld_for_the_ean() {
        let html = r#"<script type="application/ld+json">{"@graph":[
          {"@type":"Product","name":"AMD Ryzen 7 7800X3D","sku":3930054,
           "mpn":"100-100000910WOF","gtin13":"0730143314930",
           "brand":{"@type":"Brand","name":"Amd"},
           "image":"https://www.multitronic.fi/images/prod/x.webp",
           "offers":[{"@type":"Offer","price":"359.90","availability":"https://schema.org/InStock",
                      "url":"https://www.multitronic.fi/fi/products/3930054"}]}]}
        </script>"#;

        let product = parse_product_page(html, "3930054").unwrap();
        assert_eq!(product.ean.as_deref(), Some("0730143314930"));
        assert_eq!(product.sku.as_deref(), Some("100-100000910WOF"));
        assert_eq!(product.brand.as_deref(), Some("Amd"));
        assert_eq!(product.price_euro, 359.90);
        assert_eq!(product.in_stock, Some(true));
        assert_eq!(product.url, "https://www.multitronic.fi/fi/products/3930054");
    }

    #[test]
    fn a_product_without_a_gtin_still_parses() {
        let html = r#"<script type="application/ld+json">
          {"@type":"Product","name":"Refurbished build","offers":{"price":"499.00"}}
        </script>"#;
        let product = parse_product_page(html, "42").unwrap();
        assert_eq!(product.ean, None);
        assert_eq!(product.price_euro, 499.00);
        assert_eq!(product.url, "https://www.multitronic.fi/fi/products/42");
    }

    #[test]
    fn extracts_the_id_from_a_product_url() {
        assert_eq!(
            MultitronicSource::extract_id_from_url(
                "https://www.multitronic.fi/fi/products/3930054/amd-ryzen"
            ),
            Some("3930054".to_string())
        );
        assert_eq!(
            MultitronicSource::extract_id_from_url("https://www.multitronic.fi/fi/products/3930054"),
            Some("3930054".to_string())
        );
        assert_eq!(
            MultitronicSource::extract_id_from_url("https://www.multitronic.fi/fi/"),
            None
        );
    }
}
