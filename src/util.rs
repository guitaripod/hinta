/// Percent-encodes a string for use in a URL query value.
///
/// Encodes per UTF-8 byte rather than per `char`, so Finnish characters such as
/// `ä` (two bytes in UTF-8) survive the round trip instead of being truncated to
/// a single byte.
pub fn urlencode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for byte in s.as_bytes() {
        let c = *byte as char;
        if c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | '~') {
            out.push(c);
        } else if c == ' ' {
            out.push('+');
        } else {
            out.push_str(&format!("%{:02X}", byte));
        }
    }
    out
}

/// Parses a price out of retailer markup into euros.
///
/// Finnish shops write `1 299,00 €`, Nordic shops sometimes write `899,-`, and
/// a few emit English-style `1299.00`. Where both separators appear the last one
/// is the decimal point; a lone `.` is read as a decimal point only when exactly
/// two digits follow it, otherwise it is a thousands separator.
pub fn parse_price(raw: &str) -> Option<f64> {
    let cleaned: String = raw
        .chars()
        .filter(|c| c.is_ascii_digit() || *c == ',' || *c == '.')
        .collect();
    if cleaned.is_empty() || !cleaned.chars().any(|c| c.is_ascii_digit()) {
        return None;
    }

    let last_comma = cleaned.rfind(',');
    let last_dot = cleaned.rfind('.');

    let decimal_at = match (last_comma, last_dot) {
        (Some(c), Some(d)) => Some(c.max(d)),
        (Some(c), None) => separator_is_decimal(&cleaned, c).then_some(c),
        (None, Some(d)) => separator_is_decimal(&cleaned, d).then_some(d),
        (None, None) => None,
    };

    let normalized: String = cleaned
        .char_indices()
        .filter_map(|(i, ch)| match ch {
            ',' | '.' if Some(i) == decimal_at => Some('.'),
            ',' | '.' => None,
            c => Some(c),
        })
        .collect();

    let trimmed = normalized.trim_end_matches('.');
    trimmed
        .parse::<f64>()
        .ok()
        .filter(|v| v.is_finite() && *v >= 0.0)
}

/// Decides whether the separator at `idx` is a decimal point or a thousands mark.
///
/// A group of exactly three trailing digits is the unambiguous signature of a
/// thousands separator (`1.299`, `1,299`); anything else on a retail price tag
/// is a decimal point.
fn separator_is_decimal(cleaned: &str, idx: usize) -> bool {
    let trailing = cleaned.len() - idx - 1;
    trailing != 3
}

/// Resolves a possibly-relative URL found in markup against a site origin.
pub fn absolute_url(origin: &str, href: &str) -> String {
    let href = href.trim();
    if href.starts_with("http://") || href.starts_with("https://") {
        href.to_string()
    } else if let Some(rest) = href.strip_prefix("//") {
        format!("https://{}", rest)
    } else if href.starts_with('/') {
        format!("{}{}", origin.trim_end_matches('/'), href)
    } else {
        format!("{}/{}", origin.trim_end_matches('/'), href)
    }
}

/// Collapses runs of whitespace (including non-breaking spaces) into single spaces.
pub fn squeeze_whitespace(s: &str) -> String {
    s.chars()
        .map(|c| if c.is_whitespace() || c == '\u{a0}' { ' ' } else { c })
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

/// Interprets a Finnish/Swedish/English availability phrase as a stock flag.
///
/// Returns `None` when the phrase carries no stock signal at all, so callers can
/// distinguish "unknown" from a definite "out of stock".
pub fn parse_stock_phrase(raw: &str) -> Option<bool> {
    let s = raw.to_lowercase();
    if s.trim().is_empty() {
        return None;
    }
    // Checked before the in-stock phrases, so a specific "orderable from the
    // supplier" wins over the bare "saatavilla" it contains.
    const OUT: &[&str] = &[
        "saatavilla toimittajalta",
        "tilattavissa toimittajalta",
        "loppuunmyyty",
        "loppu varastosta",
        "ei varastossa",
        "tilapäisesti loppu",
        "tilapaisesti loppu",
        "ei saatavilla",
        "out of stock",
        "sold out",
        "not available",
        "ej i lager",
        "saapuu",
        "ennakkotilaus",
    ];
    const IN: &[&str] = &[
        "varastossa",
        "heti",
        "saatavilla",
        "toimitusaika",
        "in stock",
        "available",
        "i lager",
        "noudettavissa",
        "myymälässä",
        "myymalassa",
    ];
    if OUT.iter().any(|needle| s.contains(needle)) {
        return Some(false);
    }
    if IN.iter().any(|needle| s.contains(needle)) {
        return Some(true);
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn urlencode_preserves_finnish_characters_as_utf8_bytes() {
        assert_eq!(urlencode("näytönohjain"), "n%C3%A4yt%C3%B6nohjain");
        assert_eq!(urlencode("7800x3d"), "7800x3d");
        assert_eq!(urlencode("intel core i9"), "intel+core+i9");
        assert_eq!(urlencode("a&b=c"), "a%26b%3Dc");
    }

    #[test]
    fn urlencode_roundtrips_through_a_real_decoder() {
        let encoded = urlencode("hämärä ääliö");
        let decoded: String = {
            let bytes = encoded.as_bytes();
            let mut out: Vec<u8> = Vec::new();
            let mut i = 0;
            while i < bytes.len() {
                match bytes[i] {
                    b'%' => {
                        let hex = std::str::from_utf8(&bytes[i + 1..i + 3]).unwrap();
                        out.push(u8::from_str_radix(hex, 16).unwrap());
                        i += 3;
                    }
                    b'+' => {
                        out.push(b' ');
                        i += 1;
                    }
                    b => {
                        out.push(b);
                        i += 1;
                    }
                }
            }
            String::from_utf8(out).unwrap()
        };
        assert_eq!(decoded, "hämärä ääliö");
    }

    #[test]
    fn parse_price_handles_finnish_formatting() {
        assert_eq!(parse_price("1 299,00 €"), Some(1299.00));
        assert_eq!(parse_price("459,90 €"), Some(459.90));
        assert_eq!(parse_price("1.299,00"), Some(1299.00));
        assert_eq!(parse_price("899,-"), Some(899.0));
        assert_eq!(parse_price("\u{a0}79,95\u{a0}€"), Some(79.95));
    }

    #[test]
    fn parse_price_handles_english_formatting() {
        assert_eq!(parse_price("1299.00"), Some(1299.00));
        assert_eq!(parse_price("1,299.00"), Some(1299.00));
        assert_eq!(parse_price("59.99"), Some(59.99));
    }

    #[test]
    fn parse_price_reads_a_lone_separator_before_three_digits_as_thousands() {
        assert_eq!(parse_price("1.299"), Some(1299.0));
        assert_eq!(parse_price("2.499"), Some(2499.0));
        assert_eq!(parse_price("1,299"), Some(1299.0));
    }

    #[test]
    fn parse_price_keeps_single_and_double_decimals() {
        assert_eq!(parse_price("459,9"), Some(459.9));
        assert_eq!(parse_price("12,50"), Some(12.50));
    }

    #[test]
    fn parse_price_rejects_junk() {
        assert_eq!(parse_price(""), None);
        assert_eq!(parse_price("ei hintaa"), None);
        assert_eq!(parse_price("€"), None);
    }

    #[test]
    fn absolute_url_resolves_every_href_shape() {
        let origin = "https://www.datatronic.fi";
        assert_eq!(
            absolute_url(origin, "https://cdn.example.com/a.jpg"),
            "https://cdn.example.com/a.jpg"
        );
        assert_eq!(absolute_url(origin, "//cdn.example.com/a.jpg"), "https://cdn.example.com/a.jpg");
        assert_eq!(absolute_url(origin, "/tuote/123"), "https://www.datatronic.fi/tuote/123");
        assert_eq!(absolute_url(origin, "tuote/123"), "https://www.datatronic.fi/tuote/123");
        assert_eq!(absolute_url("https://x.fi/", "/a"), "https://x.fi/a");
    }

    #[test]
    fn squeeze_whitespace_normalizes_nbsp_and_newlines() {
        assert_eq!(squeeze_whitespace("  a\n\t b \u{a0} c  "), "a b c");
    }

    #[test]
    fn parse_stock_phrase_reads_finnish_availability() {
        assert_eq!(parse_stock_phrase("Varastossa"), Some(true));
        assert_eq!(parse_stock_phrase("Heti toimitukseen"), Some(true));
        assert_eq!(parse_stock_phrase("Tilapäisesti loppu"), Some(false));
        assert_eq!(parse_stock_phrase("Ei varastossa"), Some(false));
        assert_eq!(parse_stock_phrase("Out of stock"), Some(false));
        assert_eq!(parse_stock_phrase("Saatavilla toimittajalta"), Some(false));
        assert_eq!(parse_stock_phrase(""), None);
        assert_eq!(parse_stock_phrase("???"), None);
    }
}
