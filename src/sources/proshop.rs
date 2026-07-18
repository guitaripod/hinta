use anyhow::{bail, Result};
use async_trait::async_trait;
use chrono::Utc;
use scraper::{ElementRef, Html, Selector};

use crate::http::{browser_headers, RetryPolicy, CHROME_UA};
use crate::transform::types::{Product, Source};

use super::{jsonld, RetailerSource};

const ORIGIN: &str = "https://www.proshop.fi";

#[derive(Debug)]
pub struct ProshopSource {
    client: reqwest::Client,
    policy: RetryPolicy,
}

impl Default for ProshopSource {
    fn default() -> Self {
        Self::new()
    }
}

impl ProshopSource {
    /// Proshop rate-limits by IP and lengthens the penalty when a blocked client
    /// keeps probing, so this source retries slowly and gives up early.
    pub fn new() -> Self {
        Self {
            client: reqwest::Client::builder()
                .user_agent(CHROME_UA)
                .default_headers(browser_headers("fi-FI,fi;q=0.9,en;q=0.8"))
                .use_rustls_tls()
                .cookie_store(true)
                .timeout(std::time::Duration::from_secs(30))
                .build()
                .expect("static client configuration is valid"),
            policy: RetryPolicy::new(
                2,
                std::time::Duration::from_secs(10),
                std::time::Duration::from_secs(60),
            ),
        }
    }
}

struct CardSelectors {
    card: Selector,
    name: Selector,
    link: Selector,
    price: Selector,
    id_input: Selector,
    image: Selector,
    stock_icon: Selector,
    stock_text: Selector,
}

impl CardSelectors {
    fn new() -> Self {
        Self {
            card: Selector::parse("li.site-productlist-item").unwrap(),
            name: Selector::parse("h2[product-display-name]").unwrap(),
            link: Selector::parse("a.site-product-link").unwrap(),
            // Deliberately not `.price-container`, whose text concatenates the
            // VAT-inclusive and ex-VAT figures into one unparseable string.
            price: Selector::parse("span.site-currency-lg").unwrap(),
            id_input: Selector::parse("input[name=productId]").unwrap(),
            image: Selector::parse("img").unwrap(),
            stock_icon: Selector::parse(".site-stock-icon").unwrap(),
            stock_text: Selector::parse(".site-stock-text").unwrap(),
        }
    }
}

fn id_from_href(href: &str) -> Option<String> {
    let trimmed = href.split(['?', '#']).next()?.trim_end_matches('/');
    let last = trimmed.rsplit('/').next()?;
    (!last.is_empty() && last.chars().any(|c| c.is_ascii_alphanumeric()))
        .then(|| last.to_string())
}

/// Reads stock from the icon class, which is stable, and falls back to the
/// human-readable text when the markup changes.
fn card_stock(card: &ElementRef, selectors: &CardSelectors) -> Option<bool> {
    if let Some(icon) = card.select(&selectors.stock_icon).next() {
        let class = icon.value().attr("class").unwrap_or_default();
        if class.contains("site-icon-stock-in") {
            return Some(true);
        }
        if class.contains("site-icon-stock-comming") || class.contains("site-icon-stock-out") {
            return Some(false);
        }
    }
    card.select(&selectors.stock_text)
        .next()
        .map(|el| el.text().collect::<String>())
        .and_then(|text| crate::util::parse_stock_phrase(&text))
}

fn parse_card(card: &ElementRef, selectors: &CardSelectors) -> Option<Product> {
    let link = card.select(&selectors.link).next()?;
    let href = link.value().attr("href")?;
    let name = crate::util::squeeze_whitespace(
        &card
            .select(&selectors.name)
            .next()?
            .text()
            .collect::<String>(),
    );
    if name.is_empty() {
        return None;
    }

    let price = card
        .select(&selectors.price)
        .next()
        .map(|el| el.text().collect::<String>())
        .and_then(|text| crate::util::parse_price(&text))?;

    // Demo and clearance units omit the basket input, so the URL carries the id.
    let id = card
        .select(&selectors.id_input)
        .next()
        .and_then(|el| el.value().attr("value"))
        .map(str::to_string)
        .filter(|v| !v.trim().is_empty())
        .or_else(|| id_from_href(href))?;

    let image_url = card
        .select(&selectors.image)
        .next()
        .and_then(|img| {
            img.value()
                .attr("src")
                .or_else(|| img.value().attr("data-src"))
        })
        .map(|src| crate::util::absolute_url(ORIGIN, src));

    Some(Product {
        id,
        name,
        price_euro: price,
        source: Source::Proshop,
        url: crate::util::absolute_url(ORIGIN, href),
        image_url,
        in_stock: card_stock(card, selectors),
        ean: None,
        sku: None,
        brand: None,
        scraped_at: Utc::now(),
    })
}

pub(crate) fn parse_search_page(html: &str, limit: usize) -> Vec<Product> {
    let doc = Html::parse_document(html);
    let selectors = CardSelectors::new();
    doc.select(&selectors.card)
        .filter_map(|card| parse_card(&card, &selectors))
        .take(limit)
        .collect()
}

pub(crate) fn parse_product_page(html: &str, fallback_id: &str, url: &str) -> Option<Product> {
    let ld = jsonld::find_product(html)?;
    let name = ld.name?;
    let id = ld.sku.clone().unwrap_or_else(|| fallback_id.to_string());

    // The JSON-LD `image` is emitted host-less (`https:/Images/...`), so the
    // Open Graph image is used instead.
    let image_url = Html::parse_document(html)
        .select(&Selector::parse(r#"meta[property="og:image"]"#).unwrap())
        .next()
        .and_then(|el| el.value().attr("content"))
        .map(|src| crate::util::absolute_url(ORIGIN, src));

    Some(Product {
        id,
        name,
        price_euro: ld.price_euro.unwrap_or(0.0),
        source: Source::Proshop,
        url: ld.url.unwrap_or_else(|| url.to_string()),
        image_url,
        in_stock: ld.in_stock,
        ean: ld.gtin,
        sku: ld.mpn,
        brand: ld.brand,
        scraped_at: Utc::now(),
    })
}

#[async_trait]
impl RetailerSource for ProshopSource {
    fn source(&self) -> Source {
        Source::Proshop
    }

    async fn search(&self, query: &str, limit: usize) -> Result<Vec<Product>> {
        let url = format!("{}/?s={}", ORIGIN, crate::util::urlencode(query));
        let response =
            crate::http::get_with_retry(&self.client, &url, self.policy, Some(ORIGIN)).await?;

        let status = response.status();
        if status.as_u16() == 429 || status.as_u16() == 403 {
            bail!(
                "proshop.fi rate-limited this client (HTTP {}). It throttles per IP and \
                 extends the penalty while blocked — wait several minutes before retrying.",
                status.as_u16()
            );
        }
        if !status.is_success() {
            bail!("proshop.fi search returned HTTP {}", status.as_u16());
        }
        Ok(parse_search_page(&response.text().await?, limit))
    }

    async fn get_product(&self, product_id: &str) -> Result<Option<Product>> {
        // A bare id 301-redirects to the canonical slug URL, which reqwest follows.
        let (url, id) = if product_id.starts_with("http") {
            let id = id_from_href(product_id).unwrap_or_default();
            (product_id.to_string(), id)
        } else {
            (
                format!("{}/{}", ORIGIN, product_id),
                product_id.to_string(),
            )
        };

        let response =
            crate::http::get_with_retry(&self.client, &url, self.policy, Some(ORIGIN)).await?;
        let status = response.status();
        if status.as_u16() == 429 {
            bail!("proshop.fi rate-limited this client (HTTP 429); wait several minutes");
        }
        if matches!(status.as_u16(), 404 | 410) {
            return Ok(None);
        }
        if !status.is_success() {
            bail!("proshop.fi product page returned HTTP {}", status.as_u16());
        }

        let final_url = response.url().to_string();
        Ok(parse_product_page(&response.text().await?, &id, &final_url))
    }

    async fn get_category_products(&self, category_id: &str, limit: usize) -> Result<Vec<Product>> {
        self.search(category_id, limit).await
    }

    fn extract_id_from_url(url: &str) -> Option<String> {
        id_from_href(url)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const CARD: &str = r#"
    <ul><li class="site-productlist-item site-customerCenterCard position-relative" product>
      <div class="mr-2">
        <a class="show" href="/CPU/AMD-Ryzen-7-7800X3D-CPU/3140870">
          <img alt="CPU" height="116" src="/Images/174x116/3140870_f168cffc1977.png" width="174" />
        </a>
      </div>
      <div class="site-productTextContainer position-relative">
        <a class="site-product-link" href="/CPU/AMD-Ryzen-7-7800X3D-CPU/3140870">
          <h2 product-display-name >AMD Ryzen 7 7800X3D CPU - 8 ydint&#228; - 4.2 GHz - AMD AM5</h2>
          <div>Prosessori (CPU), 4.2 GHz</div>
        </a>
      </div>
      <div class="priceContainer">
        <div class="mt-2 price-container mb-2">
          <span class="site-currency-lg  has-presales-price">359,90 &#8364;</span>
          <span class="hidden-xs">(286,77 &#8364;)</span>
        </div>
        <form action="/Basket/AddItem" method="post">
          <input name="productId" type="hidden" value="3140870" />
        </form>
      </div>
      <div class="d-flex site-stock mt-2">
        <div class="site-stock-icon site-icon-stock-in"></div>
        <div class="site-stock-text">Varastossa - 2-5 arkip&#228;iv&#228;n toimitus</div>
      </div>
    </li></ul>"#;

    #[test]
    fn parses_a_search_card() {
        let products = parse_search_page(CARD, 10);
        assert_eq!(products.len(), 1);

        let p = &products[0];
        assert_eq!(p.id, "3140870");
        assert_eq!(p.name, "AMD Ryzen 7 7800X3D CPU - 8 ydintä - 4.2 GHz - AMD AM5");
        assert_eq!(p.price_euro, 359.90);
        assert_eq!(p.in_stock, Some(true));
        assert_eq!(p.url, "https://www.proshop.fi/CPU/AMD-Ryzen-7-7800X3D-CPU/3140870");
        assert_eq!(
            p.image_url.as_deref(),
            Some("https://www.proshop.fi/Images/174x116/3140870_f168cffc1977.png")
        );
    }

    #[test]
    fn reads_the_vat_inclusive_price_not_the_parenthesised_one() {
        let products = parse_search_page(CARD, 10);
        assert_eq!(products[0].price_euro, 359.90);
    }

    #[test]
    fn handles_a_non_breaking_space_thousands_separator() {
        let html = CARD.replace("359,90 &#8364;", "1&#160;949,90 &#8364;");
        let products = parse_search_page(&html, 10);
        assert_eq!(products[0].price_euro, 1949.90);
    }

    #[test]
    fn falls_back_to_the_url_id_when_the_basket_input_is_absent() {
        let html = CARD.replace(r#"<input name="productId" type="hidden" value="3140870" />"#, "");
        let products = parse_search_page(&html, 10);
        assert_eq!(products[0].id, "3140870");
    }

    #[test]
    fn reads_a_lazy_loaded_image() {
        let html = CARD.replace(
            r#"src="/Images/174x116/3140870_f168cffc1977.png""#,
            r#"data-src="/Images/174x116/lazy.png""#,
        );
        let products = parse_search_page(&html, 10);
        assert_eq!(
            products[0].image_url.as_deref(),
            Some("https://www.proshop.fi/Images/174x116/lazy.png")
        );
    }

    #[test]
    fn reads_incoming_stock_as_out_of_stock() {
        let html = CARD.replace("site-icon-stock-in", "site-icon-stock-comming");
        let products = parse_search_page(&html, 10);
        assert_eq!(products[0].in_stock, Some(false));
    }

    #[test]
    fn a_card_without_a_price_is_skipped() {
        let html = CARD.replace(r#"<span class="site-currency-lg  has-presales-price">359,90 &#8364;</span>"#, "");
        assert!(parse_search_page(&html, 10).is_empty());
    }

    #[test]
    fn an_empty_result_page_yields_nothing() {
        assert!(parse_search_page("<html><body>Ei tuloksia</body></html>", 10).is_empty());
    }

    #[test]
    fn parses_the_detail_page_json_ld_and_prefers_the_og_image() {
        let html = r#"<html><head>
          <meta property="og:image" content="https://www.proshop.fi/Images/big/3140870.png">
          <script type="application/ld+json">{"@type":"Product",
            "name":"AMD Ryzen 7 7800X3D CPU","sku":"3140870","gtin12":"730143314930",
            "mpn":"100-100000910WOF","brand":{"@type":"Brand","name":"AMD"},
            "image":"https:/Images/broken.png",
            "offers":{"@type":"Offer","price":"359.90","priceCurrency":"EUR",
                      "availability":"https://schema.org/InStock"}}</script>
          </head><body></body></html>"#;

        let product =
            parse_product_page(html, "3140870", "https://www.proshop.fi/CPU/x/3140870").unwrap();
        assert_eq!(product.id, "3140870");
        assert_eq!(product.ean.as_deref(), Some("730143314930"));
        assert_eq!(product.sku.as_deref(), Some("100-100000910WOF"));
        assert_eq!(product.brand.as_deref(), Some("AMD"));
        assert_eq!(product.price_euro, 359.90);
        assert_eq!(product.in_stock, Some(true));
        assert_eq!(
            product.image_url.as_deref(),
            Some("https://www.proshop.fi/Images/big/3140870.png")
        );
    }

    #[test]
    fn keeps_the_redirected_url_when_json_ld_omits_it() {
        let html = r#"<script type="application/ld+json">{"@type":"Product","name":"Thing"}</script>"#;
        let product =
            parse_product_page(html, "1", "https://www.proshop.fi/CPU/slug/1").unwrap();
        assert_eq!(product.url, "https://www.proshop.fi/CPU/slug/1");
    }

    #[test]
    fn extracts_the_id_from_a_product_url() {
        assert_eq!(
            ProshopSource::extract_id_from_url("https://www.proshop.fi/CPU/AMD-Ryzen/3140870"),
            Some("3140870".to_string())
        );
        assert_eq!(
            ProshopSource::extract_id_from_url("https://www.proshop.fi/CPU/Demo/8071882d"),
            Some("8071882d".to_string())
        );
    }
}
