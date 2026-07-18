use scraper::{Html, Selector};
use serde_json::Value;

/// A schema.org `Product` node lifted out of a page's JSON-LD.
///
/// Retailers that publish this give us EAN, MPN and brand for free — fields that
/// are almost never present in search-result markup but are what cross-retailer
/// matching depends on.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct LdProduct {
    pub name: Option<String>,
    pub sku: Option<String>,
    pub mpn: Option<String>,
    pub gtin: Option<String>,
    pub brand: Option<String>,
    pub image: Option<String>,
    pub url: Option<String>,
    pub price_euro: Option<f64>,
    pub in_stock: Option<bool>,
}

/// Extracts the first schema.org Product from any `<script type="application/ld+json">`
/// block, following `@graph` containers and arrays.
pub fn find_product(html: &str) -> Option<LdProduct> {
    let document = Html::parse_document(html);
    let selector = Selector::parse(r#"script[type="application/ld+json"]"#).ok()?;

    for script in document.select(&selector) {
        let raw = script.text().collect::<String>();
        let Ok(value) = serde_json::from_str::<Value>(&raw) else {
            continue;
        };
        if let Some(product) = search_value(&value) {
            return Some(product);
        }
    }
    None
}

fn search_value(value: &Value) -> Option<LdProduct> {
    match value {
        Value::Array(items) => items.iter().find_map(search_value),
        Value::Object(map) => {
            if let Some(graph) = map.get("@graph") {
                if let Some(found) = search_value(graph) {
                    return Some(found);
                }
            }
            if has_type(map.get("@type"), "Product") {
                return Some(parse_product(map));
            }
            None
        }
        _ => None,
    }
}

/// `@type` is a string on most sites but an array on a few, so both shapes count.
fn has_type(value: Option<&Value>, wanted: &str) -> bool {
    match value {
        Some(Value::String(s)) => s.eq_ignore_ascii_case(wanted),
        Some(Value::Array(items)) => items
            .iter()
            .any(|i| i.as_str().is_some_and(|s| s.eq_ignore_ascii_case(wanted))),
        _ => false,
    }
}

fn parse_product(map: &serde_json::Map<String, Value>) -> LdProduct {
    let offer = first_offer(map.get("offers"));

    LdProduct {
        name: text(map.get("name")),
        sku: text(map.get("sku")),
        mpn: text(map.get("mpn")),
        gtin: ["gtin13", "gtin", "gtin12", "gtin14", "gtin8"]
            .iter()
            .find_map(|key| text(map.get(*key))),
        brand: map.get("brand").and_then(named_value),
        image: map.get("image").and_then(first_string),
        url: offer
            .as_ref()
            .and_then(|o| text(o.get("url")))
            .or_else(|| text(map.get("url"))),
        price_euro: offer.as_ref().and_then(|o| {
            text(o.get("price"))
                .as_deref()
                .and_then(crate::util::parse_price)
        }),
        in_stock: offer
            .as_ref()
            .and_then(|o| text(o.get("availability")))
            .map(|a| {
                let a = a.to_lowercase();
                a.contains("instock") || a.contains("limitedavailability")
            }),
    }
}

/// `offers` may be a single object, an array, or an `AggregateOffer` wrapper.
///
/// Retailers that sell to businesses publish two offers — a consumer price
/// including VAT and a business price excluding it. The consumer offer is the
/// one a shopper actually pays, so it wins whenever it can be identified.
fn first_offer(value: Option<&Value>) -> Option<serde_json::Map<String, Value>> {
    match value? {
        Value::Object(map) => {
            if let Some(nested) = map.get("offers") {
                if let Some(inner) = first_offer(Some(nested)) {
                    return Some(inner);
                }
            }
            Some(map.clone())
        }
        Value::Array(items) => items
            .iter()
            .find(|i| is_consumer_offer(i))
            .and_then(|i| first_offer(Some(i)))
            .or_else(|| items.iter().find_map(|i| first_offer(Some(i)))),
        _ => None,
    }
}

fn is_consumer_offer(value: &Value) -> bool {
    let Some(map) = value.as_object() else {
        return false;
    };
    let public_customer = map
        .get("eligibleCustomerType")
        .and_then(|c| match c {
            Value::Object(inner) => text(inner.get("@id")),
            other => text(Some(other)),
        })
        .is_some_and(|id| id.to_lowercase().contains("public"));
    let taxed = map
        .get("priceSpecification")
        .and_then(|s| match s {
            Value::Array(items) => items.first().and_then(|i| i.as_object()).cloned(),
            Value::Object(inner) => Some(inner.clone()),
            _ => None,
        })
        .and_then(|spec| spec.get("valueAddedTaxIncluded").and_then(Value::as_bool))
        .unwrap_or(false);
    public_customer || taxed
}

/// Reads a scalar that schema.org allows to be either a string or a number.
fn text(value: Option<&Value>) -> Option<String> {
    match value? {
        Value::String(s) if !s.trim().is_empty() => Some(s.trim().to_string()),
        Value::Number(n) => Some(n.to_string()),
        _ => None,
    }
}

/// Reads a property that may be a bare string or a `{"name": ...}` node.
fn named_value(value: &Value) -> Option<String> {
    match value {
        Value::String(s) if !s.trim().is_empty() => Some(s.trim().to_string()),
        Value::Object(map) => text(map.get("name")),
        Value::Array(items) => items.iter().find_map(named_value),
        _ => None,
    }
}

fn first_string(value: &Value) -> Option<String> {
    match value {
        Value::String(s) if !s.trim().is_empty() => Some(s.trim().to_string()),
        Value::Array(items) => items.iter().find_map(first_string),
        Value::Object(map) => text(map.get("url")).or_else(|| text(map.get("contentUrl"))),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_a_product_from_a_graph_container() {
        let html = r#"
        <html><head>
        <script type="application/ld+json">{"@context":"https://schema.org"}</script>
        <script type="application/ld+json">{"@context":"https://schema.org","@graph":[
          {"@type":"BreadcrumbList","itemListElement":[]},
          {"@type":"Product","name":"AMD Ryzen 7 7800X3D 4.2 GHz, AM5 -suoritin (WOF)",
           "sku":3930054,"mpn":"100-100000910WOF","gtin13":"0730143314930",
           "brand":{"@type":"Brand","name":"Amd"},
           "image":"https://www.multitronic.fi/images/prod/2/E/100-100000910WOF-1.webp",
           "offers":[{"@type":"Offer","priceCurrency":"EUR","price":"359.90",
             "availability":"https://schema.org/InStock",
             "url":"https://www.multitronic.fi/fi/products/3930054"}]}
        ]}</script>
        </head><body></body></html>"#;

        let product = find_product(html).expect("product should be found");
        assert_eq!(product.name.as_deref(), Some("AMD Ryzen 7 7800X3D 4.2 GHz, AM5 -suoritin (WOF)"));
        assert_eq!(product.sku.as_deref(), Some("3930054"));
        assert_eq!(product.mpn.as_deref(), Some("100-100000910WOF"));
        assert_eq!(product.gtin.as_deref(), Some("0730143314930"));
        assert_eq!(product.brand.as_deref(), Some("Amd"));
        assert_eq!(product.price_euro, Some(359.90));
        assert_eq!(product.in_stock, Some(true));
        assert_eq!(product.url.as_deref(), Some("https://www.multitronic.fi/fi/products/3930054"));
    }

    #[test]
    fn reads_a_single_offer_object_and_out_of_stock() {
        let html = r#"<script type="application/ld+json">
        {"@type":"Product","name":"Thing","offers":{"@type":"Offer","price":99.5,
         "availability":"https://schema.org/OutOfStock"}}</script>"#;
        let product = find_product(html).unwrap();
        assert_eq!(product.price_euro, Some(99.5));
        assert_eq!(product.in_stock, Some(false));
    }

    #[test]
    fn reads_an_aggregate_offer_wrapper() {
        let html = r#"<script type="application/ld+json">
        {"@type":"Product","name":"Thing","offers":{"@type":"AggregateOffer",
         "offers":[{"@type":"Offer","price":"12,90","availability":"InStock"}]}}</script>"#;
        let product = find_product(html).unwrap();
        assert_eq!(product.price_euro, Some(12.90));
        assert_eq!(product.in_stock, Some(true));
    }

    #[test]
    fn accepts_an_array_valued_type_and_brand_string() {
        let html = r#"<script type="application/ld+json">
        {"@type":["Product","Thing"],"name":"Thing","brand":"Samsung",
         "image":["https://a/1.jpg","https://a/2.jpg"]}</script>"#;
        let product = find_product(html).unwrap();
        assert_eq!(product.brand.as_deref(), Some("Samsung"));
        assert_eq!(product.image.as_deref(), Some("https://a/1.jpg"));
    }

    #[test]
    fn ignores_pages_without_a_product_node() {
        let html = r#"<script type="application/ld+json">
        {"@type":"WebSite","name":"Shop"}</script>"#;
        assert_eq!(find_product(html), None);
        assert_eq!(find_product("<html><body>nothing</body></html>"), None);
    }

    #[test]
    fn survives_malformed_json_and_keeps_looking() {
        let html = r#"
        <script type="application/ld+json">{ this is not json }</script>
        <script type="application/ld+json">{"@type":"Product","name":"Recovered"}</script>"#;
        assert_eq!(find_product(html).unwrap().name.as_deref(), Some("Recovered"));
    }

    #[test]
    fn a_product_without_gtin_still_parses() {
        let html = r#"<script type="application/ld+json">
        {"@type":"Product","name":"Refurbished PC","offers":{"price":"499.00"}}</script>"#;
        let product = find_product(html).unwrap();
        assert_eq!(product.gtin, None);
        assert_eq!(product.in_stock, None);
        assert_eq!(product.price_euro, Some(499.00));
    }
}
