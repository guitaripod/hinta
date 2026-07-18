use anyhow::{bail, Result};
use async_trait::async_trait;
use chrono::Utc;
use scraper::{ElementRef, Html, Selector};

use crate::http::{RetryPolicy, CHROME_UA};
use crate::transform::types::{Product, Source};

use super::{jsonld, RetailerSource};

const ORIGIN: &str = "https://www.datatronic.fi";

#[derive(Debug)]
pub struct DatatronicSource {
    client: reqwest::Client,
    policy: RetryPolicy,
}

impl Default for DatatronicSource {
    fn default() -> Self {
        Self::new()
    }
}

impl DatatronicSource {
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
    name: Selector,
    price: Selector,
    link: Selector,
    image: Selector,
}

impl CardSelectors {
    fn new() -> Self {
        Self {
            card: Selector::parse(".product-miniature, .js-product-miniature, article[data-id-product]")
                .unwrap(),
            name: Selector::parse(
                ".product-title a, .product-name a, .h3.product-title a, .product-description a",
            )
            .unwrap(),
            price: Selector::parse(".price, .product-price, .product-price-and-shipping .price")
                .unwrap(),
            link: Selector::parse("a[href]").unwrap(),
            image: Selector::parse("img[src], img[data-src], img[data-full-size-image-url]").unwrap(),
        }
    }
}

fn parse_card(card: &ElementRef, selectors: &CardSelectors) -> Option<Product> {
    let name = crate::util::squeeze_whitespace(
        &card.select(&selectors.name).next()?.text().collect::<String>(),
    );
    if name.len() < 3 {
        return None;
    }

    let href = card.select(&selectors.link).next()?.value().attr("href")?;
    let url = crate::util::absolute_url(ORIGIN, href);

    let price = card
        .select(&selectors.price)
        .next()
        .map(|el| el.text().collect::<String>())
        .and_then(|text| crate::util::parse_price(&text))?;

    let id = card
        .value()
        .attr("data-id-product")
        .map(str::to_string)
        .or_else(|| DatatronicSource::extract_id_from_url(&url))?;

    let image_url = card
        .select(&selectors.image)
        .next()
        .and_then(|img| {
            img.value()
                .attr("src")
                .or_else(|| img.value().attr("data-src"))
                .or_else(|| img.value().attr("data-full-size-image-url"))
        })
        .map(|src| crate::util::absolute_url(ORIGIN, src));

    Some(Product {
        id,
        name,
        price_euro: price,
        source: Source::Datatronic,
        url,
        image_url,
        in_stock: None,
        ean: None,
        sku: None,
        brand: None,
        scraped_at: Utc::now(),
    })
}

pub(crate) fn parse_listing(html: &str, limit: usize) -> Vec<Product> {
    let doc = Html::parse_document(html);
    let selectors = CardSelectors::new();
    doc.select(&selectors.card)
        .filter_map(|card| parse_card(&card, &selectors))
        .take(limit)
        .collect()
}

/// Reads a microdata property, preferring `content` over rendered text.
fn microdata(doc: &Html, prop: &str) -> Option<String> {
    let selector = Selector::parse(&format!("[itemprop=\"{}\"]", prop)).ok()?;
    let element = doc.select(&selector).next()?;
    let raw = element
        .value()
        .attr("content")
        .map(str::to_string)
        .unwrap_or_else(|| element.text().collect::<String>());
    let value = crate::util::squeeze_whitespace(&raw);
    (!value.is_empty()).then_some(value)
}

/// Parses a product page, preferring JSON-LD and falling back to the microdata
/// and CSS selectors that PrestaShop themes vary on.
pub(crate) fn parse_product_page(html: &str, product_id: &str, url: &str) -> Option<Product> {
    let doc = Html::parse_document(html);
    let ld = jsonld::find_product(html);

    let name = ld
        .as_ref()
        .and_then(|l| l.name.clone())
        .or_else(|| microdata(&doc, "name"))
        .or_else(|| {
            let selector = Selector::parse("h1, .h1, .product-name, .product-title").ok()?;
            doc.select(&selector)
                .next()
                .map(|el| crate::util::squeeze_whitespace(&el.text().collect::<String>()))
        })
        .filter(|n| !n.is_empty())?;

    let price = ld
        .as_ref()
        .and_then(|l| l.price_euro)
        .or_else(|| {
            microdata(&doc, "price")
                .as_deref()
                .and_then(crate::util::parse_price)
        })
        .or_else(|| {
            let selector =
                Selector::parse(".current-price .price, .product-price .price, .product-price")
                    .ok()?;
            doc.select(&selector)
                .next()
                .map(|el| el.text().collect::<String>())
                .and_then(|t| crate::util::parse_price(&t))
        })
        .unwrap_or(0.0);

    let in_stock = ld.as_ref().and_then(|l| l.in_stock).or_else(|| {
        let selector =
            Selector::parse(".product-availability, .stock, #availability_value, .availability")
                .ok()?;
        doc.select(&selector)
            .next()
            .map(|el| el.text().collect::<String>())
            .and_then(|t| crate::util::parse_stock_phrase(&t))
    });

    let image_url = ld
        .as_ref()
        .and_then(|l| l.image.clone())
        .or_else(|| microdata(&doc, "image"))
        .or_else(|| {
            let selector = Selector::parse(".product-cover img, .js-qv-product-cover").ok()?;
            doc.select(&selector)
                .next()
                .and_then(|el| el.value().attr("src").map(str::to_string))
        })
        .map(|src| crate::util::absolute_url(ORIGIN, &src));

    Some(Product {
        id: product_id.to_string(),
        name,
        price_euro: price,
        source: Source::Datatronic,
        url: url.to_string(),
        image_url,
        in_stock,
        ean: ld
            .as_ref()
            .and_then(|l| l.gtin.clone())
            .or_else(|| microdata(&doc, "gtin13")),
        sku: ld
            .as_ref()
            .and_then(|l| l.mpn.clone())
            .or_else(|| microdata(&doc, "sku")),
        brand: ld
            .as_ref()
            .and_then(|l| l.brand.clone())
            .or_else(|| microdata(&doc, "brand")),
        scraped_at: Utc::now(),
    })
}

#[async_trait]
impl RetailerSource for DatatronicSource {
    fn source(&self) -> Source {
        Source::Datatronic
    }

    async fn search(&self, query: &str, limit: usize) -> Result<Vec<Product>> {
        let url = format!(
            "{}/haku?controller=search&s={}",
            ORIGIN,
            crate::util::urlencode(query)
        );
        let body =
            crate::http::get_text(&self.client, &url, self.policy, Some(ORIGIN), "datatronic.fi")
                .await?;
        Ok(parse_listing(&body, limit))
    }

    async fn get_product(&self, product_id: &str) -> Result<Option<Product>> {
        let (url, id) = if product_id.starts_with("http") {
            let id = Self::extract_id_from_url(product_id).unwrap_or_default();
            (product_id.to_string(), id)
        } else {
            (
                format!("{}/index.php?id_product={}&controller=product", ORIGIN, product_id),
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
            bail!("datatronic.fi product page returned HTTP {}", status.as_u16());
        }

        // The id-based URL redirects to the slug URL; storing the resolved one
        // keeps `hinta open` pointing at a human-readable page.
        let canonical = response.url().to_string();
        Ok(parse_product_page(&response.text().await?, &id, &canonical))
    }

    async fn get_category_products(&self, category_id: &str, limit: usize) -> Result<Vec<Product>> {
        let url = format!("{}/{}?page=1", ORIGIN, category_id.trim_start_matches('/'));
        let body =
            crate::http::get_text(&self.client, &url, self.policy, Some(ORIGIN), "datatronic.fi")
                .await?;
        Ok(parse_listing(&body, limit))
    }

    fn extract_id_from_url(url: &str) -> Option<String> {
        if let Some(rest) = url.split("id_product=").nth(1) {
            let id: String = rest.chars().take_while(|c| c.is_ascii_digit()).collect();
            if !id.is_empty() {
                return Some(id);
            }
        }
        let last = url.split(['?', '#']).next()?.rsplit('/').next()?;
        let id: String = last.chars().take_while(|c| c.is_ascii_digit()).collect();
        (!id.is_empty()).then_some(id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const CARD: &str = r#"
    <div><article class="product-miniature js-product-miniature" data-id-product="1234">
      <div class="thumbnail-container">
        <a href="/fi/prosessorit/1234-amd-ryzen-7-7800x3d.html">
          <img src="/img/p/1/2/1234.jpg" alt="x">
        </a>
      </div>
      <div class="product-description">
        <h3 class="h3 product-title"><a href="/fi/prosessorit/1234-amd-ryzen-7-7800x3d.html">AMD Ryzen 7 7800X3D</a></h3>
        <div class="product-price-and-shipping">
          <span class="price">359,90 €</span>
        </div>
      </div>
    </article></div>"#;

    #[test]
    fn parses_a_search_card() {
        let products = parse_listing(CARD, 10);
        assert_eq!(products.len(), 1);

        let p = &products[0];
        assert_eq!(p.id, "1234");
        assert_eq!(p.name, "AMD Ryzen 7 7800X3D");
        assert_eq!(p.price_euro, 359.90);
        assert_eq!(
            p.url,
            "https://www.datatronic.fi/fi/prosessorit/1234-amd-ryzen-7-7800x3d.html"
        );
        assert_eq!(
            p.image_url.as_deref(),
            Some("https://www.datatronic.fi/img/p/1/2/1234.jpg")
        );
    }

    #[test]
    fn a_card_without_a_price_is_skipped() {
        let html = CARD.replace(r#"<span class="price">359,90 €</span>"#, "");
        assert!(parse_listing(&html, 10).is_empty());
    }

    #[test]
    fn limit_truncates_the_card_list() {
        let html = format!("{}{}", CARD, CARD.replace("1234", "5678"));
        assert_eq!(parse_listing(&html, 1).len(), 1);
        assert_eq!(parse_listing(&html, 5).len(), 2);
    }

    #[test]
    fn an_empty_listing_yields_nothing() {
        assert!(parse_listing("<html><body>Ei tuloksia</body></html>", 10).is_empty());
    }

    #[test]
    fn prefers_json_ld_on_the_product_page() {
        let html = r#"<html><head><script type="application/ld+json">{"@type":"Product",
          "name":"AMD Ryzen 7 7800X3D","gtin13":"0730143314930","mpn":"100-100000910WOF",
          "brand":{"name":"AMD"},"image":"/img/p/1/2/1234.jpg",
          "offers":{"@type":"Offer","price":"359.90","availability":"https://schema.org/InStock"}}
          </script></head><body></body></html>"#;

        let product = parse_product_page(
            html,
            "1234",
            "https://www.datatronic.fi/fi/prosessorit/1234-amd.html",
        )
        .unwrap();
        assert_eq!(product.ean.as_deref(), Some("0730143314930"));
        assert_eq!(product.sku.as_deref(), Some("100-100000910WOF"));
        assert_eq!(product.brand.as_deref(), Some("AMD"));
        assert_eq!(product.price_euro, 359.90);
        assert_eq!(product.in_stock, Some(true));
    }

    #[test]
    fn stores_the_canonical_slug_url_rather_than_the_id_lookup_url() {
        let html = r#"<h1 itemprop="name">Thing</h1>"#;
        let product = parse_product_page(
            html,
            "1234",
            "https://www.datatronic.fi/fi/prosessorit/1234-amd.html",
        )
        .unwrap();
        assert_eq!(
            product.url,
            "https://www.datatronic.fi/fi/prosessorit/1234-amd.html"
        );
    }

    #[test]
    fn falls_back_to_microdata_when_json_ld_is_absent() {
        let html = r#"<html><body>
          <h1 itemprop="name">AMD Ryzen 7 7800X3D</h1>
          <meta itemprop="price" content="359.90">
          <span itemprop="gtin13">0730143314930</span>
          <div class="product-availability">Varastossa</div>
        </body></html>"#;

        let product = parse_product_page(html, "1234", "https://www.datatronic.fi/x").unwrap();
        assert_eq!(product.name, "AMD Ryzen 7 7800X3D");
        assert_eq!(product.price_euro, 359.90);
        assert_eq!(product.ean.as_deref(), Some("0730143314930"));
        assert_eq!(product.in_stock, Some(true));
    }

    #[test]
    fn reads_an_out_of_stock_phrase() {
        let html = r#"<h1>Thing</h1><div class="product-availability">Tilapäisesti loppu</div>"#;
        let product = parse_product_page(html, "1", "https://www.datatronic.fi/x").unwrap();
        assert_eq!(product.in_stock, Some(false));
    }

    #[test]
    fn extracts_the_id_from_both_url_shapes() {
        assert_eq!(
            DatatronicSource::extract_id_from_url(
                "https://www.datatronic.fi/index.php?id_product=1234&controller=product"
            ),
            Some("1234".to_string())
        );
        assert_eq!(
            DatatronicSource::extract_id_from_url(
                "https://www.datatronic.fi/fi/prosessorit/1234-amd-ryzen.html"
            ),
            Some("1234".to_string())
        );
        assert_eq!(
            DatatronicSource::extract_id_from_url("https://www.datatronic.fi/fi/"),
            None
        );
    }
}
