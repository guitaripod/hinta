use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;

use crate::transform::types::{Product, Source};

/// How two listings were judged to be the same product.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MatchBasis {
    Ean,
    Sku,
    Model,
    Name,
}

impl MatchBasis {
    pub fn label(&self) -> &'static str {
        match self {
            MatchBasis::Ean => "ean",
            MatchBasis::Sku => "sku",
            MatchBasis::Model => "model",
            MatchBasis::Name => "name",
        }
    }
}

/// The identity evidence extracted from a single listing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Signature {
    pub ean: Option<String>,
    pub sku: Option<String>,
    pub brand: Option<String>,
    /// Tokens that identify the model itself (`7800x3d`, `4090`, `sn850x`).
    pub model_tokens: BTreeSet<String>,
    /// Canonicalized capacity and size tokens (`cap:1000gb`, `240hz`) — these
    /// separate variants of one product line rather than identifying it.
    pub spec_tokens: BTreeSet<String>,
    pub name_tokens: BTreeSet<String>,
    /// Attributes that must agree exactly, such as `ti`, `pro` or `heatsink`.
    pub qualifiers: BTreeSet<String>,
    pub kind: ProductKind,
}

/// What sort of thing a listing is.
///
/// A search for `televisio` returns wall mounts, HDMI cables and installation
/// services alongside actual televisions. Classifying them lets a caller ask for
/// the devices without hand-writing a blocklist at the call site.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProductKind {
    #[default]
    Device,
    Accessory,
    Service,
}

/// Finnish compounds the head noun onto the end — `seinäteline` (wall mount),
/// `antennikaapeli` (antenna cable), `kasauspalvelu` (assembly service) — so
/// these are matched as substrings rather than whole tokens.
const ACCESSORY_STEMS: &[&str] = &[
    "teline", "jalusta", "kiinnike", "kaapeli", "johto", "adapteri", "kaukosaadin", "suojakalvo",
];

/// English words matched whole, because as substrings they collide with real
/// product words — `stand` appears inside `standard`.
const ACCESSORY_WORDS: &[&str] = &[
    "mount", "bracket", "stand", "cable", "adapter", "remote", "cord",
];

const SERVICE_STEMS: &[&str] = &["palvelu", "asennus", "huolto", "takuu", "vakuutus"];

const SERVICE_WORDS: &[&str] = &["service", "installation", "warranty", "insurance"];

fn matches_vocabulary(tokens: &[String], stems: &[&str], words: &[&str]) -> bool {
    tokens.iter().any(|token| {
        words.contains(&token.as_str()) || stems.iter().any(|stem| token.contains(stem))
    })
}

/// A listing that quotes a *range* of screen sizes fits many devices, so it is a
/// mount or bracket rather than a device of any one size.
fn states_a_size_range(expanded: &str, spec_tokens: &BTreeSet<String>) -> bool {
    let inch_measurements = spec_tokens.iter().filter(|t| t.ends_with("inch")).count();
    if inch_measurements >= 2 {
        return true;
    }

    let bytes = expanded.as_bytes();
    for (i, b) in bytes.iter().enumerate() {
        if *b != b'-' {
            continue;
        }
        let left_is_digit = i > 0 && bytes[i - 1].is_ascii_digit();
        if !left_is_digit {
            continue;
        }
        let rest = &expanded[i + 1..];
        let digits = rest.len() - rest.trim_start_matches(|c: char| c.is_ascii_digit()).len();
        if digits > 0 && rest[digits..].starts_with("inch") {
            return true;
        }
    }
    false
}

fn classify(expanded: &str, tokens: &[String], spec_tokens: &BTreeSet<String>) -> ProductKind {
    if matches_vocabulary(tokens, SERVICE_STEMS, SERVICE_WORDS) {
        return ProductKind::Service;
    }
    if matches_vocabulary(tokens, ACCESSORY_STEMS, ACCESSORY_WORDS)
        || states_a_size_range(expanded, spec_tokens)
    {
        return ProductKind::Accessory;
    }
    ProductKind::Device
}

/// The structured facts the matcher already had to work out in order to compare
/// listings, surfaced so callers can filter and display them.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Attributes {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub brand: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub screen_inches: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub capacity_gb: Option<i64>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub qualifiers: Vec<String>,
    pub kind: ProductKind,
}

impl Signature {
    pub fn screen_inches(&self) -> Option<u32> {
        self.spec_tokens
            .iter()
            .filter_map(|t| t.strip_suffix("inch"))
            .filter_map(|v| v.parse().ok())
            .max()
    }

    pub fn capacity_gb(&self) -> Option<i64> {
        self.spec_tokens
            .iter()
            .filter_map(|t| t.strip_prefix("cap:"))
            .filter_map(|v| v.strip_suffix("gb"))
            .filter_map(|v| v.parse().ok())
            .max()
    }

    pub fn attributes(&self) -> Attributes {
        Attributes {
            brand: self.brand.clone(),
            screen_inches: self.screen_inches(),
            capacity_gb: self.capacity_gb(),
            qualifiers: self.qualifiers.iter().cloned().collect(),
            kind: self.kind,
        }
    }
}

/// Folds Nordic diacritics and lowercases, so `Näytönohjain` and `naytonohjain`
/// tokenize identically across retailers that disagree on encoding.
pub fn fold_text(s: &str) -> String {
    s.chars()
        .flat_map(|c| {
            let lower = c.to_lowercase().next().unwrap_or(c);
            match lower {
                'ä' | 'å' | 'á' | 'à' | 'â' => vec!['a'],
                'ö' | 'ø' | 'ó' | 'ò' | 'ô' => vec!['o'],
                'ü' | 'ú' | 'ù' => vec!['u'],
                'é' | 'è' | 'ê' => vec!['e'],
                'í' | 'ì' => vec!['i'],
                'ß' => vec!['s', 's'],
                other => vec![other],
            }
        })
        .collect()
}

/// Normalizes a GTIN to 13 digits, rejecting anything whose check digit fails.
///
/// Retailers put all sorts of internal references in EAN fields; validating the
/// checksum keeps a bogus value from silently merging two unrelated products.
pub fn normalize_ean(raw: &str) -> Option<String> {
    let digits: String = raw.chars().filter(|c| c.is_ascii_digit()).collect();
    let padded = match digits.len() {
        8 => digits.clone(),
        12 => format!("0{}", digits),
        13 => digits.clone(),
        14 => digits[1..].to_string(),
        _ => return None,
    };
    if !has_valid_gtin_checksum(&padded) {
        return None;
    }
    Some(padded)
}

fn has_valid_gtin_checksum(digits: &str) -> bool {
    let values: Vec<u32> = digits.chars().filter_map(|c| c.to_digit(10)).collect();
    if values.len() != digits.len() || values.len() < 8 {
        return false;
    }
    let (body, check) = values.split_at(values.len() - 1);
    let sum: u32 = body
        .iter()
        .rev()
        .enumerate()
        .map(|(i, d)| if i % 2 == 0 { d * 3 } else { *d })
        .sum();
    (10 - (sum % 10)) % 10 == check[0]
}

fn normalize_sku(raw: &str) -> Option<String> {
    let cleaned: String = fold_text(raw)
        .chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .collect();
    (cleaned.len() >= 4).then_some(cleaned)
}

fn normalize_brand(raw: &str) -> Option<String> {
    let cleaned: String = fold_text(raw)
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == ' ')
        .collect();
    let trimmed = cleaned.trim().to_string();
    (!trimmed.is_empty()).then_some(trimmed)
}

/// Storage units and their size in gigabytes.
///
/// Finnish retailers write capacities in Finnish — `Tt` is *teratavu*, `Gt` is
/// *gigatavu* — while others use `TB`/`GB` and some spell it out as `1000 GB`.
/// Normalizing all of them onto one scale is what stops a 1 TB drive from being
/// grouped with a 4 TB one.
const STORAGE_UNITS: &[(&str, f64)] = &[
    ("kb", 1e-6),
    ("kt", 1e-6),
    ("mb", 1e-3),
    ("mt", 1e-3),
    ("gb", 1.0),
    ("gt", 1.0),
    ("tb", 1000.0),
    ("tt", 1000.0),
];

/// Measurement units mapped to a canonical spelling.
///
/// Screen size is the discriminator that matters most for televisions and
/// monitors, and retailers write it three ways: `55"`, `55 tuuman` and `55 inch`.
/// All of them have to land on the same key.
const MEASURE_UNITS: &[(&str, &str)] = &[
    ("hz", "hz"),
    ("khz", "khz"),
    ("mhz", "mhz"),
    ("ghz", "ghz"),
    ("w", "w"),
    ("wh", "wh"),
    ("mm", "mm"),
    ("cm", "cm"),
    ("inch", "inch"),
    ("in", "inch"),
    ("tuuma", "inch"),
    ("tuuman", "inch"),
    ("tuumaa", "inch"),
];

fn is_unit(token: &str) -> bool {
    STORAGE_UNITS.iter().any(|(u, _)| *u == token)
        || MEASURE_UNITS.iter().any(|(u, _)| *u == token)
}

/// Rewrites an inch mark into a unit word so it survives tokenization.
///
/// `55"` would otherwise split on the quote and leave a bare `55`, which is too
/// short to be a model token and carries no unit — so a 55-inch and a 65-inch
/// television would compare as the same product.
fn expand_inch_marks(folded: &str) -> String {
    let mut out = String::with_capacity(folded.len());
    let mut previous_was_digit = false;
    for c in folded.chars() {
        match c {
            '"' | '\u{201d}' | '\u{2033}' if previous_was_digit => out.push_str("inch"),
            _ => {
                previous_was_digit = c.is_ascii_digit();
                out.push(c);
            }
        }
    }
    out
}

/// Reduces a measurement token to a canonical form, so `1tb`, `1tt` and
/// `1000gb` all collapse to the same key.
fn canonical_spec(token: &str) -> Option<String> {
    let digits_end = token.find(|c: char| !c.is_ascii_digit())?;
    if digits_end == 0 {
        return None;
    }
    let value: f64 = token[..digits_end].parse().ok()?;
    let unit = &token[digits_end..];

    if let Some((_, gb_factor)) = STORAGE_UNITS.iter().find(|(u, _)| *u == unit) {
        return Some(format!("cap:{}gb", (value * gb_factor).round() as i64));
    }
    if let Some((_, canonical)) = MEASURE_UNITS.iter().find(|(u, _)| *u == unit) {
        return Some(format!("{}{}", value.round() as i64, canonical));
    }
    None
}

/// Words that name a distinct product within a line rather than describing one.
///
/// `RTX 4070`, `4070 Ti` and `4070 Super` share every model token that matters,
/// as do `990 PRO` and `990 EVO Plus`. Retailers never omit these — they are
/// part of the official product name — so treating them as required-to-agree is
/// safe, whereas inconsistently stated packaging words like `WOF` or `boxed`
/// would cause false splits and are deliberately excluded.
const LINE_QUALIFIERS: &[&str] = &[
    "ti", "super", "xt", "xtx", "pro", "evo", "plus", "max", "ultra", "lite", "mini", "nano", "se",
];

/// Cooling-accessory wording, which marks a genuinely different SKU. Finnish
/// retailers each pick their own compound — `jäähdytyssiili`, `jäähdytyselementti`,
/// `jäähdytyslevy` — so the shared stem is matched rather than each full word.
const HEATSINK_MARKERS: &[&str] = &["heatsink", "jaahdyty", "cooler"];

/// Attributes that must agree before two listings can be the same product.
///
/// An unmarked listing is taken to be the plain variant, which biases towards
/// splitting rather than merging: a false split shows one offer too few, while a
/// false merge advertises a cheapest price that does not exist.
fn qualifiers(folded_name: &str, tokens: &[String]) -> BTreeSet<String> {
    let mut found: BTreeSet<String> = tokens
        .iter()
        .filter(|t| LINE_QUALIFIERS.contains(&t.as_str()))
        .cloned()
        .collect();

    let has_heatsink = HEATSINK_MARKERS.iter().any(|m| folded_name.contains(m));
    // "ilman jäähdytyssiiliä" — *without* a heatsink — names the part it lacks.
    let negated = folded_name.contains("ilman ") || folded_name.contains("without ");
    if has_heatsink && !negated {
        found.insert("heatsink".to_string());
    }
    found
}

/// Words that carry no identifying power and would otherwise inflate name
/// similarity between unrelated listings.
const STOPWORDS: &[&str] = &[
    "the", "and", "for", "with", "new", "ja", "tai", "musta", "valkoinen", "black", "white",
    "tuote", "product", "kpl", "pcs", "gaming", "pc", "tietokone", "computer", "oem", "box",
    "retail", "bulk", "v2", "gen",
];

/// Splits a product name into unit-aware tokens.
///
/// A bare number immediately followed by a unit word is rejoined (`1 TB` →
/// `1tb`) so that retailers who space their capacities still collide with those
/// who do not.
fn tokenize(name: &str) -> Vec<String> {
    let folded = expand_inch_marks(&fold_text(name));
    let raw: Vec<String> = folded
        .split(|c: char| !c.is_ascii_alphanumeric())
        .filter(|t| !t.is_empty())
        .map(|t| t.to_string())
        .collect();

    let mut tokens: Vec<String> = Vec::with_capacity(raw.len());
    let mut i = 0;
    while i < raw.len() {
        let current = &raw[i];
        let next_is_unit = raw.get(i + 1).is_some_and(|n| is_unit(n));
        if current.chars().all(|c| c.is_ascii_digit()) && next_is_unit {
            tokens.push(format!("{}{}", current, raw[i + 1]));
            i += 2;
        } else {
            tokens.push(current.clone());
            i += 1;
        }
    }
    tokens
}

fn is_spec_token(token: &str) -> bool {
    canonical_spec(token).is_some()
}

/// A token identifies a model when it mixes letters and digits (`7800x3d`) or is
/// a standalone number long enough to be a model designation (`4090`).
fn is_model_token(token: &str) -> bool {
    if is_spec_token(token) {
        return false;
    }
    let has_digit = token.chars().any(|c| c.is_ascii_digit());
    let has_alpha = token.chars().any(|c| c.is_ascii_alphabetic());
    if !has_digit {
        return false;
    }
    if has_alpha {
        token.len() >= 2
    } else {
        token.len() >= 3
    }
}

pub fn signature(product: &Product) -> Signature {
    let expanded = expand_inch_marks(&fold_text(&product.name));
    let tokens = tokenize(&product.name);

    let model_tokens: BTreeSet<String> = tokens
        .iter()
        .filter(|t| is_model_token(t))
        .cloned()
        .collect();
    let spec_tokens: BTreeSet<String> =
        tokens.iter().filter_map(|t| canonical_spec(t)).collect();
    // Measurements enter the name set in canonical form too, so a listing
    // saying `1000 GB` and one saying `1 Tt` agree on that word as well as on
    // the capacity check.
    let name_tokens: BTreeSet<String> = tokens
        .iter()
        .filter(|t| t.len() > 1 && !STOPWORDS.contains(&t.as_str()))
        .map(|t| canonical_spec(t).unwrap_or_else(|| t.clone()))
        .collect();

    let brand = product
        .brand
        .as_deref()
        .and_then(normalize_brand)
        .or_else(|| {
            tokens
                .first()
                .filter(|t| t.chars().all(|c| c.is_ascii_alphabetic()) && t.len() > 2)
                .cloned()
        });

    Signature {
        ean: product.ean.as_deref().and_then(normalize_ean),
        sku: product.sku.as_deref().and_then(normalize_sku),
        brand,
        model_tokens,
        kind: classify(&expanded, &tokens, &spec_tokens),
        spec_tokens,
        name_tokens,
        qualifiers: qualifiers(&expanded, &tokens),
    }
}

/// Overlap coefficient: shared tokens over the *smaller* set.
///
/// Retailers describe the same product at wildly different verbosity — one
/// writes `Ryzen 7 7800X3D`, another `AMD Ryzen 7 7800X3D 4.2GHz AM5 suoritin`.
/// Jaccard punishes that asymmetry because the extra words inflate the union;
/// containment asks the question that matters instead: does everything the
/// shorter listing says also appear in the longer one?
fn containment(a: &BTreeSet<String>, b: &BTreeSet<String>) -> f64 {
    let smaller = a.len().min(b.len());
    if smaller == 0 {
        return 0.0;
    }
    a.intersection(b).count() as f64 / smaller as f64
}

/// How much identifying weight the shared model tokens carry.
///
/// A shared `7800x3d` pins down an exact product; a shared `am5` only says the
/// two listings mention the same socket. Scaling by the longest shared token
/// keeps a platform code from merging a CPU with a motherboard.
fn shared_model_specificity(a: &BTreeSet<String>, b: &BTreeSet<String>) -> f64 {
    let longest = a
        .intersection(b)
        .map(|token| token.len())
        .max()
        .unwrap_or(0);
    (longest as f64 / 6.0).min(1.0)
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Verdict {
    /// Hard evidence that these are different products; no score can override it.
    Incompatible,
    Score(f64, MatchBasis),
}

/// Judges whether two listings describe the same product.
///
/// Evidence is applied strongest-first: a validated EAN is decisive in both
/// directions, then SKU, then model tokens. Disjoint model tokens veto a merge
/// even when the prose overlaps almost entirely — `Ryzen 7 7800X3D` and
/// `Ryzen 9 7950X` share every word except the one that matters.
pub fn compare_signatures(a: &Signature, b: &Signature) -> Verdict {
    if let (Some(ea), Some(eb)) = (&a.ean, &b.ean) {
        return if ea == eb {
            Verdict::Score(1.0, MatchBasis::Ean)
        } else {
            Verdict::Incompatible
        };
    }

    if let (Some(sa), Some(sb)) = (&a.sku, &b.sku) {
        if sa == sb {
            return Verdict::Score(0.97, MatchBasis::Sku);
        }
    }

    if a.qualifiers != b.qualifiers {
        return Verdict::Incompatible;
    }

    let specs_conflict = !a.spec_tokens.is_empty()
        && !b.spec_tokens.is_empty()
        && a.spec_tokens.is_disjoint(&b.spec_tokens);
    if specs_conflict {
        return Verdict::Incompatible;
    }

    let models_conflict = !a.model_tokens.is_empty()
        && !b.model_tokens.is_empty()
        && a.model_tokens.is_disjoint(&b.model_tokens);
    if models_conflict {
        return Verdict::Incompatible;
    }

    let model_score = containment(&a.model_tokens, &b.model_tokens)
        * shared_model_specificity(&a.model_tokens, &b.model_tokens);
    let name_score = containment(&a.name_tokens, &b.name_tokens);
    let brand_score = match (&a.brand, &b.brand) {
        (Some(x), Some(y)) if x == y => 1.0,
        (Some(_), Some(_)) => 0.0,
        _ => 0.5,
    };

    let basis = if model_score > 0.0 {
        MatchBasis::Model
    } else {
        MatchBasis::Name
    };
    let score = 0.60 * model_score + 0.30 * name_score + 0.10 * brand_score;
    Verdict::Score(score, basis)
}

pub const DEFAULT_THRESHOLD: f64 = 0.55;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Offer {
    pub source: Source,
    pub product_id: String,
    pub name: String,
    pub price_euro: f64,
    pub url: String,
    pub in_stock: Option<bool>,
    pub ean: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProductGroup {
    pub name: String,
    pub matched_on: MatchBasis,
    pub confidence: f64,
    pub offers: Vec<Offer>,
    pub cheapest_price_euro: f64,
    pub highest_price_euro: f64,
    /// Absolute spread between the cheapest and dearest offer, in euros.
    pub savings_euro: f64,
    pub retailer_count: usize,
    pub attributes: Attributes,
}

/// Constraints applied to listings before they are grouped.
///
/// Filtering first rather than last means an excluded listing cannot drag an
/// unrelated product into a group, and the reported cheapest price always refers
/// to something that passed the filter.
#[derive(Debug, Clone, Default)]
pub struct Filters {
    pub min_price: Option<f64>,
    pub max_price: Option<f64>,
    pub in_stock_only: bool,
    pub min_inches: Option<u32>,
    pub max_inches: Option<u32>,
    pub brand: Option<String>,
    /// Drop mounts, cables and installation services.
    pub devices_only: bool,
}

impl Filters {
    pub fn is_noop(&self) -> bool {
        self.min_price.is_none()
            && self.max_price.is_none()
            && !self.in_stock_only
            && self.min_inches.is_none()
            && self.max_inches.is_none()
            && self.brand.is_none()
            && !self.devices_only
    }

    pub fn accepts(&self, product: &Product, sig: &Signature) -> bool {
        if let Some(min) = self.min_price {
            if product.price_euro < min {
                return false;
            }
        }
        if let Some(max) = self.max_price {
            if product.price_euro > max {
                return false;
            }
        }
        // Unknown stock is not treated as in stock; a comparison that promises
        // availability it cannot confirm is worse than one offer short.
        if self.in_stock_only && product.in_stock != Some(true) {
            return false;
        }
        if self.devices_only && sig.kind != ProductKind::Device {
            return false;
        }
        if let Some(brand) = &self.brand {
            let wanted = fold_text(brand);
            let matches_brand = sig
                .brand
                .as_ref()
                .is_some_and(|b| b.contains(&wanted))
                || sig.name_tokens.contains(&wanted);
            if !matches_brand {
                return false;
            }
        }
        if self.min_inches.is_some() || self.max_inches.is_some() {
            let Some(inches) = sig.screen_inches() else {
                return false;
            };
            if self.min_inches.is_some_and(|min| inches < min) {
                return false;
            }
            if self.max_inches.is_some_and(|max| inches > max) {
                return false;
            }
        }
        true
    }
}

/// Applies filters, reporting how many listings were dropped so a caller can
/// tell "nothing matched" from "nothing was found".
pub fn apply_filters(products: Vec<Product>, filters: &Filters) -> (Vec<Product>, usize) {
    if filters.is_noop() {
        return (products, 0);
    }
    let before = products.len();
    let kept: Vec<Product> = products
        .into_iter()
        .filter(|p| filters.accepts(p, &signature(p)))
        .collect();
    let dropped = before - kept.len();
    (kept, dropped)
}

struct Cluster {
    members: Vec<(Product, Signature)>,
    basis: MatchBasis,
    confidence: f64,
}

/// Groups listings from many retailers into one entry per distinct product.
///
/// Uses agglomerative assignment rather than union-find: a listing joins a
/// cluster only if it is compatible with *every* existing member. Transitive
/// merging would otherwise chain `A~B` and `B~C` into a single group even when
/// `A` and `C` carry contradictory EANs.
pub fn group_products(products: Vec<Product>, threshold: f64) -> Vec<ProductGroup> {
    let mut clusters: Vec<Cluster> = Vec::new();

    for product in products {
        let sig = signature(&product);
        let mut best: Option<(usize, f64, MatchBasis)> = None;

        for (idx, cluster) in clusters.iter().enumerate() {
            let mut total = 0.0;
            let mut basis = MatchBasis::Name;
            let mut compatible = true;

            for (_, member_sig) in &cluster.members {
                match compare_signatures(&sig, member_sig) {
                    Verdict::Incompatible => {
                        compatible = false;
                        break;
                    }
                    Verdict::Score(score, member_basis) => {
                        if score < threshold {
                            compatible = false;
                            break;
                        }
                        total += score;
                        if (member_basis as u8) < (basis as u8) {
                            basis = member_basis;
                        }
                    }
                }
            }

            if compatible {
                let mean = total / cluster.members.len() as f64;
                if best.is_none_or(|(_, best_score, _)| mean > best_score) {
                    best = Some((idx, mean, basis));
                }
            }
        }

        match best {
            Some((idx, score, basis)) => {
                let cluster = &mut clusters[idx];
                cluster.members.push((product, sig));
                cluster.confidence = cluster.confidence.min(score);
                if (basis as u8) < (cluster.basis as u8) {
                    cluster.basis = basis;
                }
            }
            None => clusters.push(Cluster {
                members: vec![(product, sig)],
                basis: MatchBasis::Name,
                confidence: 1.0,
            }),
        }
    }

    while merge_one_compatible_pair(&mut clusters, threshold) {}

    let mut groups: Vec<ProductGroup> = clusters.into_iter().map(build_group).collect();
    groups.sort_by(|a, b| {
        b.retailer_count
            .cmp(&a.retailer_count)
            .then(a.cheapest_price_euro.total_cmp(&b.cheapest_price_euro))
    });
    groups
}

/// Whether every listing in one cluster is compatible with every listing in the
/// other, yielding the weakest score across those pairs.
fn clusters_compatible(a: &Cluster, b: &Cluster, threshold: f64) -> Option<(f64, MatchBasis)> {
    let mut weakest = f64::MAX;
    let mut basis = MatchBasis::Name;

    for (_, left) in &a.members {
        for (_, right) in &b.members {
            match compare_signatures(left, right) {
                Verdict::Incompatible => return None,
                Verdict::Score(score, pair_basis) => {
                    if score < threshold {
                        return None;
                    }
                    weakest = weakest.min(score);
                    if (pair_basis as u8) < (basis as u8) {
                        basis = pair_basis;
                    }
                }
            }
        }
    }
    Some((weakest, basis))
}

/// Merges the first mutually compatible pair of clusters, reporting whether it
/// found one.
///
/// First-fit assignment depends on the order listings arrive in, which is just
/// the order retailers happen to respond. Without this pass the same product can
/// end up split across two groups purely because an early cluster absorbed a
/// listing that later ones could not reach.
fn merge_one_compatible_pair(clusters: &mut Vec<Cluster>, threshold: f64) -> bool {
    for i in 0..clusters.len() {
        for j in (i + 1)..clusters.len() {
            let Some((score, basis)) = clusters_compatible(&clusters[i], &clusters[j], threshold)
            else {
                continue;
            };
            let absorbed = clusters.remove(j);
            let target = &mut clusters[i];
            target.members.extend(absorbed.members);
            target.confidence = target.confidence.min(absorbed.confidence).min(score);
            if (basis as u8) < (target.basis as u8) {
                target.basis = basis;
            }
            if (absorbed.basis as u8) < (target.basis as u8) {
                target.basis = absorbed.basis;
            }
            return true;
        }
    }
    false
}

fn build_group(cluster: Cluster) -> ProductGroup {
    let mut offers: Vec<Offer> = cluster
        .members
        .iter()
        .map(|(p, sig)| Offer {
            source: p.source.clone(),
            product_id: p.id.clone(),
            name: p.name.clone(),
            price_euro: p.price_euro,
            url: p.url.clone(),
            in_stock: p.in_stock,
            ean: sig.ean.clone(),
        })
        .collect();
    offers.sort_by(|a, b| a.price_euro.total_cmp(&b.price_euro));

    let cheapest = offers.first().map(|o| o.price_euro).unwrap_or(0.0);
    let highest = offers.last().map(|o| o.price_euro).unwrap_or(0.0);

    let retailer_count = offers
        .iter()
        .map(|o| o.source.name())
        .collect::<BTreeSet<_>>()
        .len();

    let name = cluster
        .members
        .iter()
        .map(|(p, _)| p.name.clone())
        .min_by_key(|n| n.len())
        .unwrap_or_default();

    // Attributes are taken from the richest signature in the group, since one
    // retailer often states a capacity or size the others leave out.
    let attributes = cluster
        .members
        .iter()
        .map(|(_, sig)| sig.attributes())
        .max_by_key(|a| {
            a.screen_inches.is_some() as u8
                + a.capacity_gb.is_some() as u8
                + a.brand.is_some() as u8
        })
        .unwrap_or_default();

    ProductGroup {
        name,
        matched_on: if cluster.members.len() == 1 {
            MatchBasis::Name
        } else {
            cluster.basis
        },
        confidence: if cluster.members.len() == 1 {
            1.0
        } else {
            cluster.confidence
        },
        offers,
        cheapest_price_euro: cheapest,
        highest_price_euro: highest,
        savings_euro: (highest - cheapest).max(0.0),
        retailer_count,
        attributes,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;

    fn product(source: Source, id: &str, name: &str, price: f64) -> Product {
        Product {
            id: id.to_string(),
            name: name.to_string(),
            price_euro: price,
            source,
            url: format!("https://example.fi/{}", id),
            image_url: None,
            in_stock: Some(true),
            ean: None,
            sku: None,
            brand: None,
            scraped_at: Utc::now(),
        }
    }

    fn with_ean(mut p: Product, ean: &str) -> Product {
        p.ean = Some(ean.to_string());
        p
    }

    #[test]
    fn normalize_ean_validates_the_check_digit() {
        assert_eq!(normalize_ean("4711081234567"), None);
        assert_eq!(
            normalize_ean("0730143314350"),
            Some("0730143314350".to_string())
        );
        assert_eq!(normalize_ean("730143314350"), Some("0730143314350".to_string()));
    }

    #[test]
    fn normalize_ean_strips_formatting_and_rejects_short_junk() {
        assert_eq!(
            normalize_ean("0730-1433-14350"),
            Some("0730143314350".to_string())
        );
        assert_eq!(normalize_ean("12345"), None);
        assert_eq!(normalize_ean(""), None);
        assert_eq!(normalize_ean("SKU-ABC"), None);
    }

    #[test]
    fn fold_text_normalizes_finnish_diacritics() {
        assert_eq!(fold_text("Näytönohjain"), "naytonohjain");
        assert_eq!(fold_text("ÄÖÅ"), "aoa");
    }

    #[test]
    fn tokenize_rejoins_spaced_capacities() {
        assert_eq!(tokenize("Samsung 990 Pro 1 TB"), vec!["samsung", "990", "pro", "1tb"]);
        assert_eq!(tokenize("Samsung 990 Pro 1TB"), vec!["samsung", "990", "pro", "1tb"]);
    }

    #[test]
    fn tokenize_splits_hyphenated_model_numbers() {
        assert_eq!(
            tokenize("Intel Core i5-13600K"),
            vec!["intel", "core", "i5", "13600k"]
        );
    }

    #[test]
    fn model_and_spec_tokens_are_separated() {
        assert!(is_model_token("7800x3d"));
        assert!(is_model_token("4090"));
        assert!(is_model_token("sn850x"));
        assert!(!is_model_token("pro"));
        assert!(!is_model_token("1tb"));
        assert!(is_spec_token("1tb"));
        assert!(is_spec_token("240hz"));
        assert!(!is_spec_token("7800x3d"));
    }

    #[test]
    fn identical_eans_match_regardless_of_naming() {
        let a = signature(&with_ean(
            product(Source::Datatronic, "1", "AMD Ryzen 7 7800X3D", 399.0),
            "0730143314350",
        ));
        let b = signature(&with_ean(
            product(Source::Gigantti, "2", "AMD suoritin Ryzen 7 7800X3D AM5", 429.0),
            "730143314350",
        ));
        assert_eq!(compare_signatures(&a, &b), Verdict::Score(1.0, MatchBasis::Ean));
    }

    #[test]
    fn differing_eans_are_incompatible_even_with_identical_names() {
        let a = signature(&with_ean(
            product(Source::Datatronic, "1", "AMD Ryzen 7 7800X3D", 399.0),
            "0730143314350",
        ));
        let b = signature(&with_ean(
            product(Source::Gigantti, "2", "AMD Ryzen 7 7800X3D", 429.0),
            "4062313512348",
        ));
        assert_eq!(compare_signatures(&a, &b), Verdict::Incompatible);
    }

    #[test]
    fn different_models_of_one_line_never_merge() {
        let a = signature(&product(Source::Datatronic, "1", "AMD Ryzen 7 7800X3D suoritin", 399.0));
        let b = signature(&product(Source::Gigantti, "2", "AMD Ryzen 9 7950X suoritin", 519.0));
        assert_eq!(compare_signatures(&a, &b), Verdict::Incompatible);
    }

    #[test]
    fn finnish_capacity_units_normalize_onto_the_english_scale() {
        assert_eq!(canonical_spec("1tt"), Some("cap:1000gb".to_string()));
        assert_eq!(canonical_spec("1tb"), Some("cap:1000gb".to_string()));
        assert_eq!(canonical_spec("1000gb"), Some("cap:1000gb".to_string()));
        assert_eq!(canonical_spec("1000gt"), Some("cap:1000gb".to_string()));
        assert_eq!(canonical_spec("512gb"), Some("cap:512gb".to_string()));
        assert_eq!(canonical_spec("2tt"), Some("cap:2000gb".to_string()));
        assert_eq!(canonical_spec("240hz"), Some("240hz".to_string()));
        assert_eq!(canonical_spec("7800x3d"), None);
    }

    #[test]
    fn a_finnish_capacity_matches_the_same_size_written_in_english() {
        let finnish = signature(&product(
            Source::Multitronic,
            "1",
            "Samsung 990 PRO 1 Tt M.2 PCIe 4.0 NVMe -SSD-levy",
            239.90,
        ));
        let english = signature(&product(
            Source::Jimms,
            "2",
            "Samsung 1TB 990 PRO, PCIe 4.0 NVMe M.2 2280 SSD-levy",
            249.90,
        ));
        assert_eq!(finnish.spec_tokens, english.spec_tokens);
        match compare_signatures(&finnish, &english) {
            Verdict::Score(score, _) => assert!(score >= DEFAULT_THRESHOLD, "score {}", score),
            Verdict::Incompatible => panic!("1 Tt and 1TB are the same capacity"),
        }
    }

    #[test]
    fn a_terabyte_never_merges_with_a_differently_sized_drive_across_languages() {
        let one_tb = signature(&product(
            Source::Jimms,
            "1",
            "Samsung 1TB 990 PRO, PCIe 4.0 NVMe M.2 2280 SSD-levy",
            249.90,
        ));
        let four_tt = signature(&product(
            Source::Verkkokauppa,
            "2",
            "Samsung 990 PRO 4 Tt M.2 NVMe -SSD-kovalevy",
            688.99,
        ));
        let thousand_gb = signature(&product(
            Source::Datatronic,
            "3",
            "Samsung 990 PRO M.2 1000 GB PCI Express 4.0 V-NAND",
            239.90,
        ));

        assert_eq!(compare_signatures(&one_tb, &four_tt), Verdict::Incompatible);
        assert_eq!(compare_signatures(&thousand_gb, &four_tt), Verdict::Incompatible);
        match compare_signatures(&one_tb, &thousand_gb) {
            Verdict::Score(_, _) => {}
            Verdict::Incompatible => panic!("1TB and 1000 GB are the same capacity"),
        }
    }

    fn quals(name: &str) -> BTreeSet<String> {
        signature(&product(Source::Jimms, "1", name, 1.0)).qualifiers
    }

    #[test]
    fn heatsink_wording_is_recognised_whichever_compound_a_retailer_uses() {
        assert!(!quals("Samsung 990 PRO 1 Tt").contains("heatsink"));
        assert!(quals("Samsung 990 PRO Heatsink 1 Tt").contains("heatsink"));
        assert!(quals("Samsung 990 PRO 1 Tt jäähdytyssiilellä").contains("heatsink"));
        assert!(quals("Samsung 990 PRO 1 Tt jäähdytyselementillä").contains("heatsink"));
        assert!(!quals("Samsung 990 Pro SSD 1TB - Ilman jäähdytyssiiliä").contains("heatsink"));
    }

    #[test]
    fn graphics_card_tiers_never_merge() {
        let base = signature(&product(Source::Jimms, "1", "ASUS GeForce RTX 4070 12GB", 599.0));
        let ti = signature(&product(Source::Jimms, "2", "ASUS GeForce RTX 4070 Ti 12GB", 799.0));
        let super_ = signature(&product(
            Source::Jimms,
            "3",
            "ASUS GeForce RTX 4070 Super 12GB",
            679.0,
        ));

        assert_eq!(compare_signatures(&base, &ti), Verdict::Incompatible);
        assert_eq!(compare_signatures(&base, &super_), Verdict::Incompatible);
        assert_eq!(compare_signatures(&ti, &super_), Verdict::Incompatible);
    }

    #[test]
    fn different_product_lines_of_one_family_never_merge() {
        let pro = signature(&product(
            Source::Verkkokauppa,
            "1",
            "Samsung 990 PRO 1 Tt M.2 NVMe -SSD-kovalevy",
            259.99,
        ));
        let evo = signature(&product(
            Source::Verkkokauppa,
            "2",
            "Samsung 990 EVO Plus 1 Tt M.2 NVMe -SSD-kovalevy",
            236.99,
        ));
        assert_eq!(compare_signatures(&pro, &evo), Verdict::Incompatible);
    }

    #[test]
    fn screen_sizes_normalize_across_every_notation_a_retailer_uses() {
        assert_eq!(canonical_spec("55inch"), Some("55inch".to_string()));
        assert_eq!(canonical_spec("55tuuman"), Some("55inch".to_string()));
        assert_eq!(canonical_spec("55in"), Some("55inch".to_string()));
        assert_eq!(expand_inch_marks("samsung 55\" u80"), "samsung 55inch u80");
        assert_eq!(
            signature(&product(Source::Power, "1", "Samsung 55\" U80 4K LED TV", 399.0)).spec_tokens,
            signature(&product(Source::Power, "2", "Samsung 55 tuuman U80 4K LED TV", 399.0))
                .spec_tokens
        );
    }

    #[test]
    fn televisions_of_different_screen_sizes_never_merge() {
        let inch55 = signature(&product(
            Source::Verkkokauppa,
            "1",
            "Samsung 55\" U80 – 4K LED TV",
            399.0,
        ));
        let inch65 = signature(&product(
            Source::Multitronic,
            "2",
            "Samsung U80 65\" 4K LED Tizen TV, 60 Hz, HDR10+",
            459.0,
        ));
        let inch75 = signature(&product(
            Source::Verkkokauppa,
            "3",
            "Samsung 75\" U80 – 4K LED TV",
            649.0,
        ));

        assert_eq!(compare_signatures(&inch55, &inch65), Verdict::Incompatible);
        assert_eq!(compare_signatures(&inch55, &inch75), Verdict::Incompatible);
        assert_eq!(compare_signatures(&inch65, &inch75), Verdict::Incompatible);
    }

    #[test]
    fn the_same_television_still_matches_across_notations() {
        let quoted = signature(&product(
            Source::Verkkokauppa,
            "1",
            "Samsung 65\" U80 – 4K LED TV",
            459.0,
        ));
        let finnish = signature(&product(
            Source::Power,
            "2",
            "Samsung 65 tuuman U80 4K LED -televisio",
            479.0,
        ));
        match compare_signatures(&quoted, &finnish) {
            Verdict::Score(score, _) => assert!(score >= DEFAULT_THRESHOLD, "score {}", score),
            Verdict::Incompatible => panic!("same TV written two ways should match"),
        }
    }

    #[test]
    fn the_heatsink_variant_is_kept_apart_from_the_plain_one() {
        let plain = signature(&product(
            Source::Multitronic,
            "1",
            "Samsung 990 PRO 1 Tt M.2 NVMe -SSD-levy",
            239.90,
        ));
        let heatsink = signature(&product(
            Source::Multitronic,
            "2",
            "Samsung 990 PRO Heatsink 1 Tt M.2 NVMe -SSD-levy",
            269.90,
        ));
        assert_eq!(compare_signatures(&plain, &heatsink), Verdict::Incompatible);
    }

    #[test]
    fn a_matching_ean_still_wins_over_the_variant_heuristic() {
        let a = signature(&with_ean(
            product(Source::Datatronic, "1", "Samsung 990 PRO 1 Tt", 239.0),
            "0730143314350",
        ));
        let b = signature(&with_ean(
            product(Source::Jimms, "2", "Samsung 990 PRO Heatsink 1TB", 249.0),
            "0730143314350",
        ));
        assert_eq!(compare_signatures(&a, &b), Verdict::Score(1.0, MatchBasis::Ean));
    }

    #[test]
    fn different_capacities_never_merge() {
        let a = signature(&product(Source::Datatronic, "1", "Samsung 990 Pro 1TB NVMe", 99.0));
        let b = signature(&product(Source::Jimms, "2", "Samsung 990 Pro 2TB NVMe", 179.0));
        assert_eq!(compare_signatures(&a, &b), Verdict::Incompatible);
    }

    #[test]
    fn the_same_product_named_differently_matches_on_model_tokens() {
        let a = signature(&product(
            Source::Datatronic,
            "1",
            "AMD Ryzen 7 7800X3D 4.2GHz AM5 suoritin",
            399.0,
        ));
        let b = signature(&product(
            Source::Jimms,
            "2",
            "AMD Ryzen 7 7800X3D prosessori",
            389.0,
        ));
        match compare_signatures(&a, &b) {
            Verdict::Score(score, basis) => {
                assert!(score >= DEFAULT_THRESHOLD, "score was {}", score);
                assert_eq!(basis, MatchBasis::Model);
            }
            Verdict::Incompatible => panic!("should have matched"),
        }
    }

    #[test]
    fn a_shared_platform_code_alone_does_not_merge_different_product_types() {
        let cpu = signature(&product(
            Source::Datatronic,
            "1",
            "AMD Ryzen 7 7800X3D AM5 suoritin",
            399.0,
        ));
        let motherboard = signature(&product(
            Source::Jimms,
            "2",
            "ASUS ROG STRIX AM5 emolevy",
            219.0,
        ));
        match compare_signatures(&cpu, &motherboard) {
            Verdict::Incompatible => {}
            Verdict::Score(score, _) => assert!(
                score < DEFAULT_THRESHOLD,
                "a shared socket must not merge a CPU with a motherboard (score {})",
                score
            ),
        }
    }

    #[test]
    fn a_verbose_listing_still_matches_a_terse_one() {
        let verbose = signature(&product(
            Source::Datatronic,
            "1",
            "AMD Ryzen 7 7800X3D 4.2GHz AM5 8-core 104MB suoritin boxed WOF",
            399.0,
        ));
        let terse = signature(&product(Source::Jimms, "2", "Ryzen 7 7800X3D", 389.0));
        match compare_signatures(&verbose, &terse) {
            Verdict::Score(score, _) => assert!(
                score >= DEFAULT_THRESHOLD,
                "verbosity should not block a match (score {})",
                score
            ),
            Verdict::Incompatible => panic!("should have matched"),
        }
    }

    #[test]
    fn unrelated_products_do_not_match() {
        let a = signature(&product(Source::Datatronic, "1", "AMD Ryzen 7 7800X3D", 399.0));
        let b = signature(&product(Source::Jimms, "2", "Logitech MX Master 3S hiiri", 89.0));
        match compare_signatures(&a, &b) {
            Verdict::Incompatible => {}
            Verdict::Score(score, _) => {
                assert!(score < DEFAULT_THRESHOLD, "score was {}", score)
            }
        }
    }

    #[test]
    fn grouping_collapses_one_product_across_retailers() {
        let products = vec![
            product(Source::Datatronic, "1", "AMD Ryzen 7 7800X3D suoritin", 399.0),
            product(Source::Jimms, "2", "AMD Ryzen 7 7800X3D prosessori", 379.0),
            product(Source::Gigantti, "3", "AMD Ryzen 7 7800X3D", 419.0),
            product(Source::Multitronic, "4", "AMD Ryzen 9 7950X suoritin", 519.0),
        ];
        let groups = group_products(products, DEFAULT_THRESHOLD);

        assert_eq!(groups.len(), 2, "expected the 7800X3D group plus the 7950X");
        let top = &groups[0];
        assert_eq!(top.retailer_count, 3);
        assert_eq!(top.cheapest_price_euro, 379.0);
        assert_eq!(top.highest_price_euro, 419.0);
        assert_eq!(top.savings_euro, 40.0);
        assert_eq!(top.offers[0].source, Source::Jimms);
    }

    #[test]
    fn grouping_keeps_capacity_variants_apart() {
        let products = vec![
            product(Source::Datatronic, "1", "Samsung 990 Pro 1TB NVMe SSD", 99.0),
            product(Source::Jimms, "2", "Samsung 990 Pro 1TB SSD", 94.0),
            product(Source::Gigantti, "3", "Samsung 990 Pro 2TB NVMe SSD", 179.0),
        ];
        let groups = group_products(products, DEFAULT_THRESHOLD);
        assert_eq!(groups.len(), 2);
        assert_eq!(groups[0].retailer_count, 2);
        assert_eq!(groups[0].cheapest_price_euro, 94.0);
    }

    #[test]
    fn grouping_does_not_chain_through_a_contradicting_ean() {
        let products = vec![
            with_ean(
                product(Source::Datatronic, "1", "AMD Ryzen 7 7800X3D", 399.0),
                "0730143314350",
            ),
            product(Source::Jimms, "2", "AMD Ryzen 7 7800X3D", 379.0),
            with_ean(
                product(Source::Gigantti, "3", "AMD Ryzen 7 7800X3D", 419.0),
                "4062313512348",
            ),
        ];
        let groups = group_products(products, DEFAULT_THRESHOLD);
        assert!(
            groups.len() >= 2,
            "listings with contradicting EANs must not share a group"
        );
        for group in &groups {
            let eans: BTreeSet<_> = group.offers.iter().filter_map(|o| o.ean.as_ref()).collect();
            assert!(eans.len() <= 1, "a group mixed two different EANs");
        }
    }

    #[test]
    fn grouping_does_not_depend_on_the_order_retailers_reply_in() {
        let listings = vec![
            product(Source::Datatronic, "1", "Samsung 990 PRO M.2 1000 GB PCIe 4.0 V-NAND", 239.90),
            product(Source::Verkkokauppa, "2", "Samsung 990 PRO 1 Tt M.2 NVMe -SSD-kovalevy", 259.99),
            product(Source::Multitronic, "3", "Samsung 990 PRO 1 Tt M.2 PCIe 4.0 NVMe -SSD-levy", 239.90),
            product(Source::Jimms, "4", "Samsung 1TB 990 PRO, PCIe 4.0 NVMe M.2 2280 SSD-levy", 249.90),
        ];

        let forward = group_products(listings.clone(), DEFAULT_THRESHOLD);
        let mut reversed_input = listings;
        reversed_input.reverse();
        let reversed = group_products(reversed_input, DEFAULT_THRESHOLD);

        assert_eq!(
            forward.len(),
            reversed.len(),
            "group count changed with input order"
        );
        assert_eq!(
            forward.len(),
            1,
            "the same 1 TB drive from four retailers should form one group, got {:#?}",
            forward.iter().map(|g| &g.name).collect::<Vec<_>>()
        );
        assert_eq!(forward[0].retailer_count, 4);
        assert_eq!(forward[0].cheapest_price_euro, 239.90);
    }

    #[test]
    fn a_merge_pass_never_joins_clusters_that_conflict() {
        let listings = vec![
            product(Source::Datatronic, "1", "Samsung 990 PRO 1 Tt SSD", 239.0),
            product(Source::Jimms, "2", "Samsung 1TB 990 PRO SSD", 249.0),
            product(Source::Multitronic, "3", "Samsung 990 PRO 2 Tt SSD", 382.0),
            product(Source::Verkkokauppa, "4", "Samsung 990 PRO 2 Tt SSD-kovalevy", 383.0),
        ];
        let groups = group_products(listings, DEFAULT_THRESHOLD);
        assert_eq!(groups.len(), 2, "1 TB and 2 TB must stay apart");
        for group in &groups {
            let capacities: BTreeSet<_> = group
                .offers
                .iter()
                .map(|o| signature(&product(o.source.clone(), "x", &o.name, 0.0)).spec_tokens)
                .collect();
            assert_eq!(capacities.len(), 1, "a group mixed two capacities");
        }
    }

    fn kind_of(name: &str) -> ProductKind {
        signature(&product(Source::Jimms, "1", name, 1.0)).kind
    }

    #[test]
    fn wall_mounts_and_cables_are_classified_as_accessories() {
        assert_eq!(kind_of("Deltaco 32-70\" Wall Mount Fixed, seinäteline näytölle"), ProductKind::Accessory);
        assert_eq!(kind_of("dezen TV Wall Mount - Slim - 37\"-80\" - Max 75kg"), ProductKind::Accessory);
        assert_eq!(kind_of("Neomounts 32-65\" Wall Mount, seinäteline"), ProductKind::Accessory);
        assert_eq!(kind_of("Alterzone 45-65\" Trio lattiajalusta, valkoinen"), ProductKind::Accessory);
        assert_eq!(kind_of("Deltaco Antennikaapeli, 75 Ohm, 2m, valkoinen"), ProductKind::Accessory);
        assert_eq!(kind_of("One For All WM 6252 TV-seinäteline, 13-43\""), ProductKind::Accessory);
    }

    #[test]
    fn installation_offerings_are_classified_as_services() {
        assert_eq!(kind_of("Tietokoneen kasauspalvelu"), ProductKind::Service);
        assert_eq!(
            kind_of("Tietokoneen kasauspalvelu sekä käyttöjärjestelmän asennus"),
            ProductKind::Service
        );
    }

    #[test]
    fn actual_televisions_are_classified_as_devices() {
        assert_eq!(kind_of("Samsung 85\" U80 – 4K LED TV"), ProductKind::Device);
        assert_eq!(kind_of("LG 65\" UA73 – 4K LED TV"), ProductKind::Device);
        assert_eq!(kind_of("Thomson 65\" UHD Google TV 65UG4S14"), ProductKind::Device);
        assert_eq!(
            kind_of("Samsung 75\" 4K LED-televisio TU85U8075HUXNA"),
            ProductKind::Device
        );
        assert_eq!(kind_of("AMD Ryzen 7 7800X3D suoritin"), ProductKind::Device);
    }

    #[test]
    fn attributes_expose_what_the_matcher_already_worked_out() {
        let tv = signature(&product(Source::Verkkokauppa, "1", "Samsung 85\" U80 – 4K LED TV", 899.0));
        assert_eq!(tv.screen_inches(), Some(85));
        assert_eq!(tv.capacity_gb(), None);
        assert_eq!(tv.attributes().kind, ProductKind::Device);

        let ssd = signature(&product(Source::Jimms, "2", "Samsung 1TB 990 PRO NVMe SSD", 249.0));
        assert_eq!(ssd.capacity_gb(), Some(1000));
        assert_eq!(ssd.screen_inches(), None);
        assert!(ssd.attributes().qualifiers.contains(&"pro".to_string()));
    }

    #[test]
    fn filters_narrow_by_price_stock_size_and_kind() {
        let listings = vec![
            product(Source::Verkkokauppa, "1", "Samsung 85\" U80 – 4K LED TV", 899.0),
            product(Source::Verkkokauppa, "2", "LG 55\" UA73 – 4K LED TV", 379.0),
            product(Source::Jimms, "3", "Deltaco 32-70\" Wall Mount Fixed, seinäteline", 9.90),
            product(Source::Datatronic, "4", "Tietokoneen kasauspalvelu", 47.94),
        ];

        let (devices, dropped) = apply_filters(
            listings.clone(),
            &Filters {
                devices_only: true,
                ..Default::default()
            },
        );
        assert_eq!(devices.len(), 2);
        assert_eq!(dropped, 2);

        let (big, _) = apply_filters(
            listings.clone(),
            &Filters {
                min_inches: Some(65),
                devices_only: true,
                ..Default::default()
            },
        );
        assert_eq!(big.len(), 1);
        assert_eq!(big[0].id, "1");

        let (cheap, _) = apply_filters(
            listings.clone(),
            &Filters {
                max_price: Some(500.0),
                devices_only: true,
                ..Default::default()
            },
        );
        assert_eq!(cheap.len(), 1);
        assert_eq!(cheap[0].id, "2");
    }

    #[test]
    fn a_size_filter_excludes_listings_that_state_no_size() {
        let listings = vec![product(Source::Jimms, "1", "AMD Ryzen 7 7800X3D", 399.0)];
        let (kept, dropped) = apply_filters(
            listings,
            &Filters {
                min_inches: Some(55),
                ..Default::default()
            },
        );
        assert!(kept.is_empty());
        assert_eq!(dropped, 1);
    }

    #[test]
    fn unknown_stock_does_not_satisfy_an_in_stock_filter() {
        let mut unknown = product(Source::Datatronic, "1", "Samsung 85\" U80 TV", 899.0);
        unknown.in_stock = None;
        let mut out = product(Source::Jimms, "2", "LG 85\" TV", 799.0);
        out.in_stock = Some(false);

        let (kept, _) = apply_filters(
            vec![unknown, out],
            &Filters {
                in_stock_only: true,
                ..Default::default()
            },
        );
        assert!(kept.is_empty());
    }

    #[test]
    fn a_noop_filter_keeps_everything_untouched() {
        let listings = vec![product(Source::Jimms, "1", "Anything", 1.0)];
        let (kept, dropped) = apply_filters(listings, &Filters::default());
        assert_eq!(kept.len(), 1);
        assert_eq!(dropped, 0);
    }

    #[test]
    fn groups_carry_the_richest_attributes_of_their_members() {
        let groups = group_products(
            vec![
                product(Source::Verkkokauppa, "1", "Samsung 85\" U80 – 4K LED TV", 899.0),
                product(Source::Power, "2", "Samsung 85 tuuman U80 4K LED -televisio", 949.0),
            ],
            DEFAULT_THRESHOLD,
        );
        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0].attributes.screen_inches, Some(85));
        assert_eq!(groups[0].attributes.kind, ProductKind::Device);
    }

    #[test]
    fn grouping_an_empty_list_yields_nothing() {
        assert!(group_products(Vec::new(), DEFAULT_THRESHOLD).is_empty());
    }

    #[test]
    fn a_lone_listing_still_forms_a_group() {
        let groups = group_products(
            vec![product(Source::Datatronic, "1", "AMD Ryzen 7 7800X3D", 399.0)],
            DEFAULT_THRESHOLD,
        );
        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0].retailer_count, 1);
        assert_eq!(groups[0].savings_euro, 0.0);
        assert_eq!(groups[0].confidence, 1.0);
    }
}
