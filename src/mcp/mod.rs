use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::io::{BufRead, BufReader, Write};

use crate::matching::{self, Filters, DEFAULT_THRESHOLD};
use crate::sources::{
    all_sources, enrich_missing_eans, info_for, searchable_sources, source_for,
};
use crate::store::Store;
use crate::transform::types::{source_from_str, Product, Source};

const PROTOCOL_VERSION: &str = "2024-11-05";

#[derive(Debug, Deserialize)]
struct McpRequest {
    #[serde(default)]
    id: Option<Value>,
    #[serde(default)]
    method: Option<String>,
    #[serde(default)]
    params: Option<McpParams>,
}

impl McpRequest {
    /// A JSON-RPC request without an id is a notification, and the spec forbids
    /// replying to one.
    fn is_notification(&self) -> bool {
        self.id.is_none()
    }
}

#[derive(Debug, Deserialize, Default)]
struct McpParams {
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    arguments: Option<Value>,
}

#[derive(Debug, Serialize)]
struct McpResponse {
    jsonrpc: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    id: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    result: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<McpError>,
}

impl McpResponse {
    fn ok(id: Option<Value>, result: Value) -> Self {
        Self {
            jsonrpc: "2.0",
            id,
            result: Some(result),
            error: None,
        }
    }

    fn err(id: Option<Value>, code: i32, message: String) -> Self {
        Self {
            jsonrpc: "2.0",
            id,
            result: None,
            error: Some(McpError { code, message }),
        }
    }
}

#[derive(Debug, Serialize)]
struct McpError {
    code: i32,
    message: String,
}

const METHOD_NOT_FOUND: i32 = -32601;

fn tool_definitions() -> Value {
    let source_names = "datatronic, verkkokauppa, power, jimms, multitronic, proshop, gigantti";

    json!([
        {
            "name": "search",
            "description": "Search products across Finnish electronics retailers. Returns a flat list of listings.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "query": {"type": "string", "description": "Search term"},
                    "limit": {"type": "integer", "description": "Max results per retailer (default 10)"},
                    "source": {"type": "string", "description": format!("Restrict to one retailer: {}", source_names)},
                    "min_price": {"type": "number"},
                    "max_price": {"type": "number"},
                    "in_stock": {"type": "boolean", "description": "Exclude listings not confirmed in stock"},
                    "min_inches": {"type": "integer", "description": "Minimum screen size"},
                    "max_inches": {"type": "integer", "description": "Maximum screen size"},
                    "brand": {"type": "string"},
                    "devices_only": {"type": "boolean", "description": "Drop mounts, cables and installation services"}
                },
                "required": ["query"]
            }
        },
        {
            "name": "compare",
            "description": "Search every retailer and group the same product across them using EAN, SKU and model-number matching. Returns one entry per distinct product with each retailer's price, so the cheapest offer is directly visible.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "query": {"type": "string", "description": "Search term to compare"},
                    "limit": {"type": "integer", "description": "Results per retailer (default 10)"},
                    "threshold": {"type": "number", "description": "Match confidence needed to group two listings, 0.0-1.0 (default 0.55)"},
                    "multi_only": {"type": "boolean", "description": "Only return products carried by more than one retailer"},
                    "enrich": {"type": "boolean", "description": "Fetch product pages to learn EANs that search results omit, turning fuzzy name matches into certain ones. Costs one request per listing."},
                    "min_price": {"type": "number"},
                    "max_price": {"type": "number"},
                    "in_stock": {"type": "boolean", "description": "Exclude listings not confirmed in stock"},
                    "min_inches": {"type": "integer", "description": "Minimum screen size"},
                    "max_inches": {"type": "integer", "description": "Maximum screen size"},
                    "brand": {"type": "string"},
                    "devices_only": {"type": "boolean", "description": "Drop mounts, cables and installation services — a search like 'televisio' otherwise returns wall brackets"}
                },
                "required": ["query"]
            }
        },
        {
            "name": "set_alert",
            "description": "Watch a product and report when it drops to a target price. Starts tracking it automatically, so a later refresh will check it.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "product_id": {"type": "string"},
                    "source": {"type": "string"},
                    "below": {"type": "number", "description": "Target price in euros"}
                },
                "required": ["product_id", "source", "below"]
            }
        },
        {
            "name": "clear_alert",
            "description": "Remove a price alert",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "product_id": {"type": "string"},
                    "source": {"type": "string"}
                },
                "required": ["product_id", "source"]
            }
        },
        {
            "name": "list_alerts",
            "description": "List price alerts with each product's current price and whether the target has been reached",
            "inputSchema": {"type": "object", "properties": {}}
        },
        {
            "name": "get_product",
            "description": "Fetch one product by id or URL, including EAN and brand where the retailer publishes them.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "id": {"type": "string", "description": "Product id or full URL"},
                    "source": {"type": "string", "description": format!("Retailer: {}", source_names)}
                },
                "required": ["id", "source"]
            }
        },
        {
            "name": "track",
            "description": "Start tracking a product so its price is recorded on each refresh",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "product_id": {"type": "string"},
                    "source": {"type": "string"}
                },
                "required": ["product_id", "source"]
            }
        },
        {
            "name": "untrack",
            "description": "Stop tracking a product",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "product_id": {"type": "string"},
                    "source": {"type": "string"}
                },
                "required": ["product_id", "source"]
            }
        },
        {
            "name": "list_tracked",
            "description": "List tracked products with their most recent price",
            "inputSchema": {"type": "object", "properties": {}}
        },
        {
            "name": "price_history",
            "description": "Return recorded price points for a product, newest first",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "product_id": {"type": "string"},
                    "source": {"type": "string"},
                    "limit": {"type": "integer", "description": "Max points (default 30)"}
                },
                "required": ["product_id", "source"]
            }
        },
        {
            "name": "refresh",
            "description": "Re-fetch every tracked product and append any price changes",
            "inputSchema": {"type": "object", "properties": {}}
        },
        {
            "name": "sources",
            "description": "List retailers with their capabilities: whether search works, whether product lookup works, whether search results carry an EAN, and any robots.txt restriction.",
            "inputSchema": {"type": "object", "properties": {}}
        },
        {
            "name": "stats",
            "description": "Summarise the local database: product, price-point and tracked counts",
            "inputSchema": {"type": "object", "properties": {}}
        }
    ])
}

pub async fn run() {
    let db_path = crate::data_dir().join("hinta.db");
    if let Some(parent) = db_path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }

    let store = match Store::open(&db_path) {
        Ok(s) => std::sync::Arc::new(s),
        Err(e) => {
            eprintln!("Failed to open database at {}: {}", db_path.display(), e);
            std::process::exit(1);
        }
    };

    let (tx, mut rx) = tokio::sync::mpsc::channel::<String>(32);

    let reader_store = store.clone();
    tokio::spawn(async move {
        let reader = BufReader::new(std::io::stdin());
        for line in reader.lines() {
            let Ok(line) = line else { break };
            let line = line.trim().to_string();
            if line.is_empty() {
                continue;
            }

            let store = reader_store.clone();
            let tx = tx.clone();
            tokio::spawn(async move {
                if let Some(response) = process_line(&store, &line).await {
                    let _ = tx.send(response).await;
                }
            });
        }
    });

    let mut stdout = std::io::stdout();
    while let Some(response) = rx.recv().await {
        let _ = writeln!(stdout, "{}", response);
        let _ = stdout.flush();
    }
}

/// Handles one line of JSON-RPC, returning the serialized reply, or `None` when
/// the message is a notification that must not be answered.
pub(crate) async fn process_line(store: &Store, line: &str) -> Option<String> {
    let request: McpRequest = match serde_json::from_str(line) {
        Ok(req) => req,
        Err(e) => {
            let response = McpResponse::err(None, -32700, format!("parse error: {}", e));
            return serde_json::to_string(&response).ok();
        }
    };

    let response = handle_request(store, &request).await?;
    serde_json::to_string(&response).ok()
}

async fn handle_request(store: &Store, request: &McpRequest) -> Option<McpResponse> {
    let method = request.method.as_deref().unwrap_or("");
    let id = request.id.clone();

    let result = match method {
        "initialize" => Ok(json!({
            "protocolVersion": PROTOCOL_VERSION,
            "capabilities": {"tools": {}},
            "serverInfo": {"name": "hinta", "version": env!("CARGO_PKG_VERSION")},
        })),
        "tools/list" => Ok(json!({ "tools": tool_definitions() })),
        "ping" => Ok(json!({})),
        "tools/call" => {
            let params = request.params.as_ref();
            let name = params.and_then(|p| p.name.as_deref()).unwrap_or("");
            let arguments = params
                .and_then(|p| p.arguments.clone())
                .unwrap_or(Value::Null);
            Ok(tool_result(store, name, &arguments).await)
        }
        _ if request.is_notification() => Ok(Value::Null),
        other => Err((METHOD_NOT_FOUND, format!("unknown method: {}", other))),
    };

    if request.is_notification() {
        return None;
    }

    Some(match result {
        Ok(value) => McpResponse::ok(id, value),
        Err((code, message)) => McpResponse::err(id, code, message),
    })
}

/// Wraps a tool outcome in MCP content, marking failures with `isError` so the
/// caller can distinguish a broken retailer from an empty result.
async fn tool_result(store: &Store, name: &str, arguments: &Value) -> Value {
    match call_tool(store, name, arguments).await {
        Ok(value) => json!({
            "content": [{"type": "text", "text": to_text(&value)}],
            "isError": false,
        }),
        Err(e) => json!({
            "content": [{"type": "text", "text": e.to_string()}],
            "isError": true,
        }),
    }
}

fn to_text(value: &Value) -> String {
    serde_json::to_string_pretty(value).unwrap_or_else(|_| value.to_string())
}

fn required_str(arguments: &Value, key: &str) -> anyhow::Result<String> {
    arguments[key]
        .as_str()
        .map(str::to_string)
        .filter(|v| !v.trim().is_empty())
        .ok_or_else(|| anyhow::anyhow!("missing required argument: {}", key))
}

fn required_source(arguments: &Value, key: &str) -> anyhow::Result<Source> {
    let raw = required_str(arguments, key)?;
    source_from_str(&raw).ok_or_else(|| anyhow::anyhow!("unknown source: {}", raw))
}

fn filters_from(arguments: &Value) -> Filters {
    Filters {
        min_price: arguments["min_price"].as_f64(),
        max_price: arguments["max_price"].as_f64(),
        in_stock_only: arguments["in_stock"].as_bool().unwrap_or(false),
        min_inches: arguments["min_inches"].as_u64().map(|v| v as u32),
        max_inches: arguments["max_inches"].as_u64().map(|v| v as u32),
        brand: arguments["brand"].as_str().map(str::to_string),
        devices_only: arguments["devices_only"].as_bool().unwrap_or(false),
    }
}

pub(crate) async fn call_tool(
    store: &Store,
    name: &str,
    arguments: &Value,
) -> anyhow::Result<Value> {
    match name {
        "search" => {
            let query = required_str(arguments, "query")?;
            let limit = arguments["limit"].as_u64().unwrap_or(10) as usize;
            let sources = match arguments["source"].as_str() {
                Some(raw) => vec![source_for(
                    &source_from_str(raw).ok_or_else(|| anyhow::anyhow!("unknown source: {}", raw))?,
                )],
                None => searchable_sources(),
            };

            let (products, errors) = fan_out(store, &sources, &query, limit).await;
            let (products, filtered_out) =
                matching::apply_filters(products, &filters_from(arguments));
            Ok(json!({
                "query": query,
                "results": products,
                "total_hits": products.len(),
                "filtered_out": filtered_out,
                "errors": errors,
            }))
        }

        "compare" => {
            let query = required_str(arguments, "query")?;
            let limit = arguments["limit"].as_u64().unwrap_or(10) as usize;
            let threshold = arguments["threshold"].as_f64().unwrap_or(DEFAULT_THRESHOLD);
            let multi_only = arguments["multi_only"].as_bool().unwrap_or(false);

            let sources = searchable_sources();
            let (products, errors) = fan_out(store, &sources, &query, limit).await;
            let (mut products, filtered_out) =
                matching::apply_filters(products, &filters_from(arguments));

            let enriched = if arguments["enrich"].as_bool().unwrap_or(false) {
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
            Ok(json!({
                "query": query,
                "groups": groups,
                "group_count": groups.len(),
                "filtered_out": filtered_out,
                "enriched": enriched,
                "errors": errors,
            }))
        }

        "set_alert" => {
            let product_id = required_str(arguments, "product_id")?;
            let source = required_source(arguments, "source")?;
            let below = arguments["below"]
                .as_f64()
                .filter(|v| *v > 0.0)
                .ok_or_else(|| anyhow::anyhow!("below must be a positive price"))?;
            store.set_alert(&product_id, &source, below)?;
            Ok(json!({"alert_set": true, "product_id": product_id,
                      "source": source.name(), "target_price": below}))
        }

        "clear_alert" => {
            let product_id = required_str(arguments, "product_id")?;
            let source = required_source(arguments, "source")?;
            let removed = store.clear_alert(&product_id, &source)?;
            Ok(json!({"alert_cleared": removed, "product_id": product_id, "source": source.name()}))
        }

        "list_alerts" => Ok(serde_json::to_value(store.list_alerts()?)?),

        "get_product" => {
            let id = required_str(arguments, "id")?;
            let source = required_source(arguments, "source")?;
            let retailer = source_for(&source);

            match retailer.get_product(&id).await? {
                Some(product) => {
                    store.record_sighting(&product)?;
                    Ok(serde_json::to_value(product)?)
                }
                None => Err(anyhow::anyhow!("{} not found on {}", id, source.name())),
            }
        }

        "track" => {
            let product_id = required_str(arguments, "product_id")?;
            let source = required_source(arguments, "source")?;
            store.track_product(&product_id, &source)?;
            Ok(json!({"tracked": true, "product_id": product_id, "source": source.name()}))
        }

        "untrack" => {
            let product_id = required_str(arguments, "product_id")?;
            let source = required_source(arguments, "source")?;
            let removed = store.untrack_product(&product_id, &source)?;
            Ok(json!({"untracked": removed, "product_id": product_id, "source": source.name()}))
        }

        "list_tracked" => {
            let tracked = store.list_tracked()?;
            let mut rows = Vec::new();
            for (product_id, source) in &tracked {
                let product = store.get_product(product_id, source)?;
                rows.push(json!({
                    "product_id": product_id,
                    "source": source.name(),
                    "name": product.as_ref().map(|p| p.name.clone()),
                    "price_euro": product.as_ref().map(|p| p.price_euro),
                    "in_stock": product.as_ref().and_then(|p| p.in_stock),
                    "url": product.as_ref().map(|p| p.url.clone()),
                }));
            }
            Ok(json!(rows))
        }

        "price_history" => {
            let product_id = required_str(arguments, "product_id")?;
            let source = required_source(arguments, "source")?;
            let limit = arguments["limit"].as_u64().unwrap_or(30) as usize;
            Ok(json!({
                "product": store.get_product(&product_id, &source)?,
                "history": store.get_price_history(&product_id, &source, limit)?,
            }))
        }

        "refresh" => {
            let tracked = store.list_tracked()?;
            let mut updated = 0usize;
            let mut changed = 0usize;
            let mut errors = Vec::new();

            for (product_id, source) in &tracked {
                match source_for(source).get_product(product_id).await {
                    Ok(Some(product)) => {
                        if store.record_sighting(&product)? {
                            changed += 1;
                        }
                        updated += 1;
                    }
                    Ok(None) => errors.push(json!({
                        "source": source.name(),
                        "error": format!("{} no longer listed", product_id),
                    })),
                    Err(e) => errors.push(json!({
                        "source": source.name(),
                        "error": e.to_string().lines().next().unwrap_or_default(),
                    })),
                }
            }
            let triggered: Vec<_> = store
                .list_alerts()?
                .into_iter()
                .filter(|a| a.triggered)
                .collect();

            Ok(json!({
                "updated": updated,
                "changed": changed,
                "total": tracked.len(),
                "alerts_triggered": triggered,
                "errors": errors,
            }))
        }

        "sources" => {
            let infos: Vec<_> = all_sources().iter().map(|s| info_for(&s.source())).collect();
            Ok(serde_json::to_value(infos)?)
        }

        "stats" => Ok(serde_json::to_value(store.stats()?)?),

        other => Err(anyhow::anyhow!("unknown tool: {}", other)),
    }
}

async fn fan_out(
    store: &Store,
    sources: &[crate::sources::RetailerSourceEnum],
    query: &str,
    limit: usize,
) -> (Vec<Product>, Vec<Value>) {
    let futures = sources.iter().map(|source| async move {
        let name = source.source().name().to_string();
        (name, source.search(query, limit).await)
    });
    let results = futures::future::join_all(futures).await;

    let mut products = Vec::new();
    let mut errors = Vec::new();
    for (name, result) in results {
        match result {
            Ok(found) => {
                for product in &found {
                    let _ = store.record_sighting(product);
                }
                products.extend(found);
            }
            Err(e) => errors.push(json!({
                "source": name,
                "error": e.to_string().lines().next().unwrap_or_default(),
            })),
        }
    }
    (products, errors)
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;

    fn store() -> Store {
        Store::open_in_memory().unwrap()
    }

    fn sample(source: Source, id: &str, name: &str, price: f64) -> Product {
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

    #[tokio::test]
    async fn initialize_reports_the_protocol_and_server() {
        let response = process_line(
            &store(),
            r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}"#,
        )
        .await
        .expect("initialize must be answered");

        let value: Value = serde_json::from_str(&response).unwrap();
        assert_eq!(value["id"], 1);
        assert_eq!(value["result"]["protocolVersion"], PROTOCOL_VERSION);
        assert_eq!(value["result"]["serverInfo"]["name"], "hinta");
        assert!(value["error"].is_null());
    }

    #[tokio::test]
    async fn a_notification_is_never_answered() {
        let response = process_line(
            &store(),
            r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#,
        )
        .await;
        assert!(response.is_none(), "notifications must not receive a reply");
    }

    #[tokio::test]
    async fn an_unknown_method_returns_method_not_found() {
        let response = process_line(&store(), r#"{"jsonrpc":"2.0","id":7,"method":"nope"}"#)
            .await
            .unwrap();
        let value: Value = serde_json::from_str(&response).unwrap();
        assert_eq!(value["error"]["code"], METHOD_NOT_FOUND);
        assert_eq!(value["id"], 7);
    }

    #[tokio::test]
    async fn malformed_json_returns_a_parse_error() {
        let response = process_line(&store(), "{not json").await.unwrap();
        let value: Value = serde_json::from_str(&response).unwrap();
        assert_eq!(value["error"]["code"], -32700);
    }

    #[tokio::test]
    async fn tools_list_advertises_every_tool_with_a_schema() {
        let response = process_line(&store(), r#"{"jsonrpc":"2.0","id":2,"method":"tools/list"}"#)
            .await
            .unwrap();
        let value: Value = serde_json::from_str(&response).unwrap();
        let tools = value["result"]["tools"].as_array().unwrap();

        assert_eq!(tools.len(), 13);
        for tool in tools {
            assert!(tool["name"].is_string());
            assert!(!tool["description"].as_str().unwrap().is_empty());
            assert_eq!(tool["inputSchema"]["type"], "object");
        }

        let names: Vec<&str> = tools.iter().map(|t| t["name"].as_str().unwrap()).collect();
        for expected in [
            "search", "compare", "get_product", "track", "untrack", "list_tracked",
            "price_history", "refresh", "sources", "stats", "set_alert", "clear_alert",
            "list_alerts",
        ] {
            assert!(names.contains(&expected), "{} tool missing", expected);
        }
    }

    #[tokio::test]
    async fn alerts_round_trip_through_tool_calls() {
        let store = store();
        store
            .record_sighting(&sample(Source::Verkkokauppa, "1", "Samsung 85\" U80 TV", 899.0))
            .unwrap();

        let set = call_tool(
            &store,
            "set_alert",
            &json!({"product_id": "1", "source": "verkkokauppa", "below": 750.0}),
        )
        .await
        .unwrap();
        assert_eq!(set["alert_set"], true);

        let listed = call_tool(&store, "list_alerts", &json!({})).await.unwrap();
        assert_eq!(listed[0]["target_price"], 750.0);
        assert_eq!(listed[0]["current_price"], 899.0);
        assert_eq!(listed[0]["triggered"], false);

        let cleared = call_tool(
            &store,
            "clear_alert",
            &json!({"product_id": "1", "source": "verkkokauppa"}),
        )
        .await
        .unwrap();
        assert_eq!(cleared["alert_cleared"], true);
    }

    #[tokio::test]
    async fn an_alert_needs_a_positive_target_price() {
        let err = call_tool(
            &store(),
            "set_alert",
            &json!({"product_id": "1", "source": "power", "below": 0}),
        )
        .await
        .unwrap_err();
        assert!(err.to_string().contains("positive"), "unexpected: {}", err);
    }

    #[tokio::test]
    async fn refresh_reports_alerts_that_have_been_reached() {
        let store = store();
        let mut product = sample(Source::Power, "1", "TV", 900.0);
        store.record_sighting(&product).unwrap();
        store.set_alert("1", &Source::Power, 800.0).unwrap();
        product.price_euro = 780.0;
        store.record_sighting(&product).unwrap();

        // Untracked so refresh fetches nothing and the test stays offline; the
        // alert is still evaluated against the stored price, which is the
        // behaviour under test.
        store.untrack_product("1", &Source::Power).unwrap();

        let result = call_tool(&store, "refresh", &json!({})).await.unwrap();
        let triggered = result["alerts_triggered"].as_array().unwrap();
        assert_eq!(triggered.len(), 1);
        assert_eq!(triggered[0]["current_price"], 780.0);
    }

    #[tokio::test]
    async fn tracking_round_trips_through_tool_calls() {
        let store = store();
        store
            .record_sighting(&sample(Source::Datatronic, "1234", "AMD Ryzen 7 7800X3D", 359.90))
            .unwrap();

        let tracked = call_tool(
            &store,
            "track",
            &json!({"product_id": "1234", "source": "datatronic"}),
        )
        .await
        .unwrap();
        assert_eq!(tracked["tracked"], true);

        let listed = call_tool(&store, "list_tracked", &json!({})).await.unwrap();
        assert_eq!(listed.as_array().unwrap().len(), 1);
        assert_eq!(listed[0]["name"], "AMD Ryzen 7 7800X3D");
        assert_eq!(listed[0]["price_euro"], 359.90);

        let removed = call_tool(
            &store,
            "untrack",
            &json!({"product_id": "1234", "source": "datatronic"}),
        )
        .await
        .unwrap();
        assert_eq!(removed["untracked"], true);
        assert!(call_tool(&store, "list_tracked", &json!({}))
            .await
            .unwrap()
            .as_array()
            .unwrap()
            .is_empty());
    }

    #[tokio::test]
    async fn price_history_returns_recorded_points() {
        let store = store();
        let mut product = sample(Source::Datatronic, "1234", "Thing", 100.0);
        store.record_sighting(&product).unwrap();
        product.price_euro = 90.0;
        store.record_sighting(&product).unwrap();

        let result = call_tool(
            &store,
            "price_history",
            &json!({"product_id": "1234", "source": "datatronic"}),
        )
        .await
        .unwrap();

        assert_eq!(result["history"].as_array().unwrap().len(), 2);
        assert_eq!(result["product"]["price_euro"], 90.0);
    }

    #[tokio::test]
    async fn a_missing_required_argument_is_reported() {
        let err = call_tool(&store(), "search", &json!({})).await.unwrap_err();
        assert!(err.to_string().contains("query"), "unexpected: {}", err);

        let err = call_tool(&store(), "track", &json!({"product_id": "1"}))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("source"), "unexpected: {}", err);
    }

    #[tokio::test]
    async fn an_unknown_source_is_rejected() {
        let err = call_tool(
            &store(),
            "track",
            &json!({"product_id": "1", "source": "amazon"}),
        )
        .await
        .unwrap_err();
        assert!(err.to_string().contains("unknown source"), "unexpected: {}", err);
    }

    #[tokio::test]
    async fn an_unknown_tool_is_rejected() {
        let err = call_tool(&store(), "teleport", &json!({})).await.unwrap_err();
        assert!(err.to_string().contains("unknown tool"), "unexpected: {}", err);
    }

    #[tokio::test]
    async fn a_failing_tool_is_marked_as_an_error_in_the_content() {
        let result = tool_result(&store(), "teleport", &json!({})).await;
        assert_eq!(result["isError"], true);
        assert!(result["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("unknown tool"));
    }

    #[tokio::test]
    async fn sources_tool_reports_capabilities_and_robots_status() {
        let result = call_tool(&store(), "sources", &json!({})).await.unwrap();
        let entries = result.as_array().unwrap();
        assert_eq!(entries.len(), 7);

        let gigantti = entries.iter().find(|e| e["id"] == "gigantti").unwrap();
        assert_eq!(gigantti["search"], false);
        assert_eq!(gigantti["product_lookup"], true);

        let verkkokauppa = entries.iter().find(|e| e["id"] == "verkkokauppa").unwrap();
        assert_eq!(verkkokauppa["search"], true);
        assert_eq!(verkkokauppa["ean_in_search"], true);
    }

    #[tokio::test]
    async fn stats_tool_summarises_the_database() {
        let store = store();
        store
            .record_sighting(&sample(Source::Datatronic, "1", "A", 10.0))
            .unwrap();

        let result = call_tool(&store, "stats", &json!({})).await.unwrap();
        assert_eq!(result["products"], 1);
        assert_eq!(result["price_points"], 1);
        assert_eq!(result["tracked"], 0);
    }

    #[tokio::test]
    async fn refresh_with_nothing_tracked_does_no_work() {
        let result = call_tool(&store(), "refresh", &json!({})).await.unwrap();
        assert_eq!(result["total"], 0);
        assert_eq!(result["updated"], 0);
    }
}
