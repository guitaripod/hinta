use clap::{Parser, Subcommand};
use serde::Serialize;
use serde_json::json;

use hinta::matching::{self, Filters, ProductGroup, DEFAULT_THRESHOLD};
use hinta::sources::{
    all_sources, enrich_missing_eans, info_for, searchable_sources, source_for, RetailerSourceEnum,
};
use hinta::mcp;
use hinta::store::Store;
use hinta::transform::types::{Product, Source};

#[derive(Parser)]
#[command(
    name = "hinta",
    about = "Agent-native CLI for Finnish electronics retailers",
    version
)]
struct Cli {
    #[command(subcommand)]
    command: Commands,

    #[arg(long, global = true, default_value_t = false)]
    json: bool,
}

#[derive(Subcommand)]
enum Commands {
    /// Search one retailer, or every searchable retailer at once
    Search {
        query: String,
        #[arg(long, default_value_t = 10)]
        limit: usize,
        #[arg(long)]
        source: Option<String>,
        #[command(flatten)]
        filters: FilterArgs,
    },

    /// Fetch a single product by id or URL
    Product {
        id: String,
        #[arg(long)]
        source: String,
    },

    /// Search every retailer and group the same product across them
    Compare {
        query: String,
        #[arg(long, default_value_t = 10)]
        limit: usize,
        /// Match confidence required to group two listings (0.0-1.0)
        #[arg(long, default_value_t = DEFAULT_THRESHOLD)]
        threshold: f64,
        /// Only show products carried by more than one retailer
        #[arg(long, default_value_t = false)]
        multi_only: bool,
        /// Fetch product pages to learn EANs the search results omit
        #[arg(long, default_value_t = false)]
        enrich: bool,
        #[command(flatten)]
        filters: FilterArgs,
    },

    /// Watch a product and report when it drops to a target price
    Alert {
        product_id: String,
        #[arg(long)]
        source: String,
        /// Notify once the price reaches this many euros or less
        #[arg(long)]
        below: f64,
    },

    /// Remove a price alert
    Unalert {
        product_id: String,
        #[arg(long)]
        source: String,
    },

    /// List price alerts and whether they have been reached
    Alerts,

    Track {
        product_id: String,
        #[arg(long)]
        source: String,
    },

    Untrack {
        product_id: String,
        #[arg(long)]
        source: String,
    },

    Tracked,

    History {
        product_id: String,
        #[arg(long)]
        source: String,
        #[arg(long, default_value_t = 30)]
        limit: usize,
    },

    /// Re-fetch every tracked product and record price changes
    Refresh {
        #[arg(long, default_value_t = 2.0)]
        delay: f64,
    },

    Open {
        id: String,
        #[arg(long)]
        source: Option<String>,
    },

    Stats,
    Sources,
    Mcp,
}

/// Shared narrowing options. Declared once so `search` and `compare` cannot
/// drift apart on what they accept.
#[derive(Debug, Clone, clap::Args)]
struct FilterArgs {
    #[arg(long)]
    min_price: Option<f64>,
    #[arg(long)]
    max_price: Option<f64>,
    /// Exclude listings that are not confirmed in stock
    #[arg(long, default_value_t = false)]
    in_stock: bool,
    /// Minimum screen size in inches
    #[arg(long)]
    min_inches: Option<u32>,
    /// Maximum screen size in inches
    #[arg(long)]
    max_inches: Option<u32>,
    #[arg(long)]
    brand: Option<String>,
    /// Drop mounts, cables and installation services
    #[arg(long, default_value_t = false)]
    devices_only: bool,
}

impl From<FilterArgs> for Filters {
    fn from(args: FilterArgs) -> Self {
        Filters {
            min_price: args.min_price,
            max_price: args.max_price,
            in_stock_only: args.in_stock,
            min_inches: args.min_inches,
            max_inches: args.max_inches,
            brand: args.brand,
            devices_only: args.devices_only,
        }
    }
}

fn parse_source(s: &str) -> anyhow::Result<Source> {
    hinta::transform::types::source_from_str(s)
        .ok_or_else(|| anyhow::anyhow!("unknown source: {} (see `hinta sources`)", s))
}

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "hinta=info".into()),
        )
        .with_target(false)
        .without_time()
        .with_writer(std::io::stderr)
        .init();

    let cli = Cli::parse();

    let db_path = hinta::data_dir().join("hinta.db");
    if let Some(parent) = db_path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }

    let store = match Store::open(&db_path) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("Failed to open database at {}: {}", db_path.display(), e);
            std::process::exit(1);
        }
    };

    if let Err(e) = run_command(cli, &store).await {
        eprintln!("Error: {}", e);
        std::process::exit(1);
    }
}

#[derive(Debug, Serialize)]
struct SourceError {
    source: String,
    error: String,
}

/// Runs a query against many retailers concurrently, keeping per-source failures
/// separate from results so one broken retailer cannot mask the others.
async fn fan_out(
    sources: &[RetailerSourceEnum],
    query: &str,
    limit: usize,
) -> (Vec<Product>, Vec<SourceError>) {
    let futures = sources.iter().map(|source| async move {
        let name = source.source().name().to_string();
        (name, source.search(query, limit).await)
    });
    let results = futures::future::join_all(futures).await;

    let mut products = Vec::new();
    let mut errors = Vec::new();
    for (name, result) in results {
        match result {
            Ok(found) => products.extend(found),
            Err(e) => errors.push(SourceError {
                source: name,
                error: first_line(&e.to_string()),
            }),
        }
    }
    (products, errors)
}

/// Retailer errors are multi-line explanations; the summary line is enough for
/// a results listing.
fn first_line(message: &str) -> String {
    message.lines().next().unwrap_or(message).to_string()
}

async fn run_command(cli: Cli, store: &Store) -> anyhow::Result<()> {
    match cli.command {
        Commands::Search {
            query,
            limit,
            source,
            filters,
        } => {
            let sources = match source {
                Some(name) => vec![source_for(&parse_source(&name)?)],
                None => searchable_sources(),
            };
            let (products, errors) = fan_out(&sources, &query, limit).await;
            for product in &products {
                let _ = store.record_sighting(product);
            }

            let (products, filtered_out) = matching::apply_filters(products, &filters.into());

            if cli.json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&json!({
                        "query": query,
                        "results": products,
                        "total_hits": products.len(),
                        "filtered_out": filtered_out,
                        "sources_searched": sources.iter().map(|s| s.source().name()).collect::<Vec<_>>(),
                        "errors": errors,
                    }))?
                );
            } else {
                print_search_results(&query, &products, filtered_out, &errors);
            }
        }

        Commands::Product { id, source } => {
            let src = parse_source(&source)?;
            let retailer = source_for(&src);

            match retailer.get_product(&id).await? {
                Some(product) => {
                    store.record_sighting(&product)?;
                    if cli.json {
                        println!("{}", serde_json::to_string_pretty(&json!({ "product": product }))?);
                    } else {
                        print_product(&product);
                    }
                }
                None => {
                    if cli.json {
                        println!(
                            "{}",
                            serde_json::to_string_pretty(&json!({
                                "product": serde_json::Value::Null,
                                "error": format!("not found on {}", src.name()),
                            }))?
                        );
                    } else {
                        eprintln!("Product {} not found on {}", id, src.name());
                    }
                    std::process::exit(2);
                }
            }
        }

        Commands::Compare {
            query,
            limit,
            threshold,
            multi_only,
            enrich,
            filters,
        } => {
            let sources = searchable_sources();
            let (mut products, errors) = fan_out(&sources, &query, limit).await;
            for product in &products {
                let _ = store.record_sighting(product);
            }

            let (mut products, filtered_out) =
                matching::apply_filters(std::mem::take(&mut products), &filters.into());

            let enriched = if enrich {
                let count = enrich_missing_eans(&mut products).await;
                for product in &products {
                    let _ = store.record_sighting(product);
                }
                count
            } else {
                0
            };

            let mut groups = matching::group_products(products, threshold);
            if multi_only {
                groups.retain(|g| g.retailer_count > 1);
            }

            if cli.json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&json!({
                        "query": query,
                        "groups": groups,
                        "group_count": groups.len(),
                        "filtered_out": filtered_out,
                        "enriched": enriched,
                        "sources_searched": sources.iter().map(|s| s.source().name()).collect::<Vec<_>>(),
                        "errors": errors,
                    }))?
                );
            } else {
                print_comparison(&query, &groups, filtered_out, &errors);
            }
        }

        Commands::Alert {
            product_id,
            source,
            below,
        } => {
            let src = parse_source(&source)?;
            if below <= 0.0 {
                anyhow::bail!("--below must be a positive price");
            }
            store.set_alert(&product_id, &src, below)?;
            if cli.json {
                println!(
                    "{}",
                    json!({"alert_set": true, "product_id": product_id,
                           "source": src.name(), "target_price": below})
                );
            } else {
                println!(
                    "Alerting when {} on {} reaches {}",
                    product_id,
                    src.name(),
                    format_price(below)
                );
            }
        }

        Commands::Unalert { product_id, source } => {
            let src = parse_source(&source)?;
            let removed = store.clear_alert(&product_id, &src)?;
            if cli.json {
                println!(
                    "{}",
                    json!({"alert_cleared": removed, "product_id": product_id, "source": src.name()})
                );
            } else if removed {
                println!("Cleared alert on {} from {}", product_id, src.name());
            } else {
                println!("No alert set on {} from {}", product_id, src.name());
            }
        }

        Commands::Alerts => {
            let alerts = store.list_alerts()?;
            if cli.json {
                println!("{}", serde_json::to_string_pretty(&alerts)?);
            } else if alerts.is_empty() {
                println!("No price alerts set. Use `hinta alert <id> --source <s> --below <price>`.");
            } else {
                for alert in &alerts {
                    let current = alert
                        .current_price
                        .map(format_price)
                        .unwrap_or_else(|| "no price yet".into());
                    println!(
                        "  {} {:>10} → target {:>10}  {:<16} {}",
                        if alert.triggered { "REACHED" } else { "waiting" },
                        current,
                        format_price(alert.target_price),
                        alert.source.name(),
                        alert.name.as_deref().unwrap_or(&alert.product_id)
                    );
                }
            }
        }

        Commands::Track { product_id, source } => {
            let src = parse_source(&source)?;
            store.track_product(&product_id, &src)?;
            if cli.json {
                println!("{}", json!({"tracked": true, "product_id": product_id, "source": src.name()}));
            } else {
                println!("Tracking {} from {}", product_id, src.name());
            }
        }

        Commands::Untrack { product_id, source } => {
            let src = parse_source(&source)?;
            let removed = store.untrack_product(&product_id, &src)?;
            if cli.json {
                println!("{}", json!({"untracked": removed, "product_id": product_id, "source": src.name()}));
            } else if removed {
                println!("Stopped tracking {} from {}", product_id, src.name());
            } else {
                println!("{} from {} was not tracked", product_id, src.name());
            }
        }

        Commands::Tracked => {
            let tracked = store.list_tracked()?;
            let mut rows = Vec::new();
            for (id, src) in &tracked {
                let product = store.get_product(id, src)?;
                rows.push(json!({
                    "product_id": id,
                    "source": src.name(),
                    "name": product.as_ref().map(|p| p.name.clone()),
                    "price_euro": product.as_ref().map(|p| p.price_euro),
                    "url": product.as_ref().map(|p| p.url.clone()),
                }));
            }

            if cli.json {
                println!("{}", serde_json::to_string_pretty(&rows)?);
            } else if rows.is_empty() {
                println!("No products tracked. Use `hinta track <id> --source <s>`.");
            } else {
                for (id, src) in &tracked {
                    let product = store.get_product(id, src)?;
                    match product {
                        Some(p) if p.price_euro > 0.0 => println!(
                            "  {:>10}  {:<16} {:<10} {}",
                            format_price(p.price_euro),
                            src.name(),
                            id,
                            p.name
                        ),
                        _ => println!("  {:>10}  {:<16} {}", "-", src.name(), id),
                    }
                }
            }
        }

        Commands::History {
            product_id,
            source,
            limit,
        } => {
            let src = parse_source(&source)?;
            let history = store.get_price_history(&product_id, &src, limit)?;
            let product = store.get_product(&product_id, &src)?;

            if cli.json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&json!({
                        "product": product,
                        "history": history,
                    }))?
                );
            } else {
                if let Some(p) = &product {
                    println!("{}", p.name);
                    println!("{}", "─".repeat(60));
                }
                if history.is_empty() {
                    println!("  No price history recorded.");
                }
                for point in history.iter().rev() {
                    println!(
                        "  {}  {}",
                        format_price(point.price_euro),
                        point.recorded_at.format("%Y-%m-%d %H:%M")
                    );
                }
            }
        }

        Commands::Refresh { delay } => {
            let delay = hinta::util::duration_from_secs(delay)
                .ok_or_else(|| anyhow::anyhow!("--delay must be a finite number of seconds"))?;
            let tracked = store.list_tracked()?;
            if tracked.is_empty() {
                if cli.json {
                    println!("{}", json!({"updated": 0, "changed": 0, "errors": [], "total": 0}));
                } else {
                    println!("No products tracked. Use `hinta track` first.");
                }
                return Ok(());
            }

            let mut updated = 0usize;
            let mut changed = 0usize;
            let mut errors = Vec::new();

            for (product_id, src) in &tracked {
                let retailer = source_for(src);
                match retailer.get_product(product_id).await {
                    Ok(Some(product)) => {
                        let moved = store.record_sighting(&product)?;
                        updated += 1;
                        if moved {
                            changed += 1;
                        }
                        if !cli.json {
                            println!(
                                "  {:>10}  {:<16} {}{}",
                                format_price(product.price_euro),
                                src.name(),
                                product.name,
                                if moved { "  (changed)" } else { "" }
                            );
                        }
                    }
                    Ok(None) => errors.push(SourceError {
                        source: src.name().to_string(),
                        error: format!("{} no longer listed", product_id),
                    }),
                    Err(e) => errors.push(SourceError {
                        source: src.name().to_string(),
                        error: first_line(&e.to_string()),
                    }),
                }

                if !delay.is_zero() {
                    tokio::time::sleep(delay).await;
                }
            }

            let triggered: Vec<_> = store
                .list_alerts()?
                .into_iter()
                .filter(|a| a.triggered)
                .collect();

            if cli.json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&json!({
                        "updated": updated,
                        "changed": changed,
                        "errors": errors,
                        "total": tracked.len(),
                        "alerts_triggered": triggered,
                    }))?
                );
            } else {
                println!(
                    "\nUpdated {} of {} tracked products ({} price changes, {} errors)",
                    updated,
                    tracked.len(),
                    changed,
                    errors.len()
                );
                for alert in &triggered {
                    println!(
                        "  PRICE REACHED  {} (target {}) — {}",
                        alert.current_price.map(format_price).unwrap_or_default(),
                        format_price(alert.target_price),
                        alert.name.as_deref().unwrap_or(&alert.product_id)
                    );
                    if let Some(url) = &alert.url {
                        println!("                 {}", url);
                    }
                }
                for error in &errors {
                    eprintln!("  {}: {}", error.source, error.error);
                }
            }
        }

        Commands::Open { id, source } => {
            let url = if id.starts_with("http") {
                id
            } else {
                let src = source
                    .ok_or_else(|| anyhow::anyhow!("provide --source when opening by product id"))?;
                let src = parse_source(&src)?;
                store
                    .get_product(&id, &src)?
                    .map(|p| p.url)
                    .ok_or_else(|| {
                        anyhow::anyhow!(
                            "{} is not in the local database; run `hinta product {} --source {}` first",
                            id,
                            id,
                            src.name()
                        )
                    })?
            };

            let _ = std::process::Command::new("xdg-open").arg(&url).spawn();
            if cli.json {
                println!("{}", json!({ "opened": url }));
            } else {
                println!("Opening {}", url);
            }
        }

        Commands::Stats => {
            let stats = store.stats()?;
            if cli.json {
                println!("{}", serde_json::to_string_pretty(&stats)?);
            } else {
                println!("Products:     {}", stats.products);
                println!("Price points: {}", stats.price_points);
                println!("Tracked:      {}", stats.tracked);
                if !stats.by_source.is_empty() {
                    println!("\nBy source:");
                    for row in &stats.by_source {
                        println!("  {:<14} {}", row.source, row.count);
                    }
                }
            }
        }

        Commands::Sources => {
            let infos: Vec<_> = all_sources().iter().map(|s| info_for(&s.source())).collect();
            if cli.json {
                println!("{}", serde_json::to_string_pretty(&infos)?);
            } else {
                for info in &infos {
                    println!("{} ({})", info.id, info.domain);
                    println!(
                        "  search: {}    product lookup: {}    ean in search: {}",
                        yes_no(info.search),
                        yes_no(info.product_lookup),
                        yes_no(info.ean_in_search)
                    );
                    println!("  transport: {}", info.transport);
                    println!("  robots:    {}", info.robots);
                    println!("  notes:     {}\n", info.notes);
                }
            }
        }

        Commands::Mcp => {
            mcp::run().await;
        }
    }

    Ok(())
}

fn yes_no(value: bool) -> &'static str {
    if value {
        "yes"
    } else {
        "no"
    }
}

fn format_price(price: f64) -> String {
    format!("{:.2} EUR", price)
}

fn stock_label(in_stock: Option<bool>) -> &'static str {
    match in_stock {
        Some(true) => "in stock",
        Some(false) => "out of stock",
        None => "stock unknown",
    }
}

fn print_search_results(
    query: &str,
    products: &[Product],
    filtered_out: usize,
    errors: &[SourceError],
) {
    if products.is_empty() {
        println!("No results for \"{}\".", query);
        report_filtering(filtered_out);
    } else {
        println!("\"{}\" — {} results\n", query, products.len());
        let mut sorted: Vec<&Product> = products.iter().collect();
        sorted.sort_by(|a, b| a.price_euro.total_cmp(&b.price_euro));
        for p in sorted {
            println!("  {:>10}  {:<16} {}", format_price(p.price_euro), p.source.name(), p.name);
            println!("  {:>10}  {}", "", p.url);
        }
        report_filtering(filtered_out);
    }
    print_errors(errors);
}

/// Renders the facts the matcher extracted, so a reader can see *why* two
/// similarly named listings were kept apart.
fn describe_attributes(attributes: &matching::Attributes) -> String {
    let mut parts = Vec::new();
    if let Some(inches) = attributes.screen_inches {
        parts.push(format!("{}\"", inches));
    }
    if let Some(gb) = attributes.capacity_gb {
        parts.push(if gb >= 1000 && gb % 1000 == 0 {
            format!("{} TB", gb / 1000)
        } else {
            format!("{} GB", gb)
        });
    }
    parts.extend(attributes.qualifiers.iter().cloned());
    if attributes.kind != matching::ProductKind::Device {
        parts.push(format!("{:?}", attributes.kind).to_lowercase());
    }
    if parts.is_empty() {
        String::new()
    } else {
        format!("  [{}]", parts.join(", "))
    }
}

/// Says how many listings a filter removed, so an empty or short result is not
/// mistaken for the retailers having nothing.
fn report_filtering(filtered_out: usize) {
    if filtered_out > 0 {
        println!("\n({} listings excluded by filters)", filtered_out);
    }
}

fn print_comparison(
    query: &str,
    groups: &[ProductGroup],
    filtered_out: usize,
    errors: &[SourceError],
) {
    if groups.is_empty() {
        println!("No results for \"{}\".", query);
        report_filtering(filtered_out);
    } else {
        println!("\"{}\" — {} distinct products\n", query, groups.len());
        for group in groups {
            println!("{}{}", group.name, describe_attributes(&group.attributes));
            if group.retailer_count > 1 {
                println!(
                    "  {} retailers · save {} · matched on {} ({:.0}% confidence)",
                    group.retailer_count,
                    format_price(group.savings_euro),
                    group.matched_on.label(),
                    group.confidence * 100.0
                );
            }
            for offer in &group.offers {
                println!(
                    "  {:>10}  {:<16} {:<13} {}",
                    format_price(offer.price_euro),
                    offer.source.name(),
                    stock_label(offer.in_stock),
                    offer.url
                );
            }
            println!();
        }
    }
    print_errors(errors);
}

fn print_errors(errors: &[SourceError]) {
    if errors.is_empty() {
        return;
    }
    eprintln!("\nUnavailable sources:");
    for error in errors {
        eprintln!("  {}: {}", error.source, error.error);
    }
}

fn print_product(p: &Product) {
    println!("{}", p.name);
    println!("{}", "─".repeat(60));
    println!("  Source:    {}", p.source.name());
    println!("  Price:     {}", format_price(p.price_euro));
    println!("  Stock:     {}", stock_label(p.in_stock));
    if let Some(brand) = &p.brand {
        println!("  Brand:     {}", brand);
    }
    if let Some(ean) = &p.ean {
        println!("  EAN:       {}", ean);
    }
    if let Some(sku) = &p.sku {
        println!("  SKU/MPN:   {}", sku);
    }
    println!("  ID:        {}", p.id);
    println!("  URL:       {}", p.url);
    if let Some(image) = &p.image_url {
        println!("  Image:     {}", image);
    }
    println!("  Scraped:   {}", p.scraped_at.format("%Y-%m-%d %H:%M UTC"));
}
