use anyhow::Result;
use chrono::{DateTime, Utc};
use rusqlite::{params, Connection};
use std::path::Path;
use std::sync::Mutex;

use crate::transform::types::{PricePoint, Product, Source};

pub struct Store {
    conn: Mutex<Connection>,
}

const PRODUCT_COLUMNS: &str = "p.id, p.source, p.name, p.url, p.image_url, p.ean, p.sku, p.brand, p.last_seen,
     (SELECT h.price_euro FROM price_history h
       WHERE h.product_id = p.id AND h.source = p.source
       ORDER BY h.recorded_at DESC LIMIT 1) AS price_euro,
     (SELECT h.in_stock FROM price_history h
       WHERE h.product_id = p.id AND h.source = p.source
       ORDER BY h.recorded_at DESC LIMIT 1) AS in_stock";

impl Store {
    pub fn open(path: &Path) -> Result<Self> {
        let conn = Connection::open(path)?;
        let store = Self {
            conn: Mutex::new(conn),
        };
        store.migrate()?;
        Ok(store)
    }

    pub fn open_in_memory() -> Result<Self> {
        let store = Self {
            conn: Mutex::new(Connection::open_in_memory()?),
        };
        store.migrate()?;
        Ok(store)
    }

    fn migrate(&self) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS products (
                id TEXT NOT NULL,
                source TEXT NOT NULL,
                name TEXT NOT NULL,
                url TEXT NOT NULL,
                image_url TEXT,
                ean TEXT,
                brand TEXT,
                first_seen TEXT NOT NULL,
                last_seen TEXT NOT NULL,
                PRIMARY KEY (id, source)
            );

            CREATE TABLE IF NOT EXISTS price_history (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                product_id TEXT NOT NULL,
                source TEXT NOT NULL,
                price_euro REAL NOT NULL,
                in_stock INTEGER,
                recorded_at TEXT NOT NULL,
                FOREIGN KEY (product_id, source) REFERENCES products(id, source)
            );

            CREATE INDEX IF NOT EXISTS idx_price_history_product
                ON price_history(product_id, source, recorded_at);

            CREATE TABLE IF NOT EXISTS tracked (
                product_id TEXT NOT NULL,
                source TEXT NOT NULL,
                added_at TEXT NOT NULL,
                PRIMARY KEY (product_id, source)
            );

            CREATE TABLE IF NOT EXISTS alerts (
                product_id TEXT NOT NULL,
                source TEXT NOT NULL,
                target_price REAL NOT NULL,
                created_at TEXT NOT NULL,
                PRIMARY KEY (product_id, source)
            );",
        )?;

        if !Self::column_exists(&conn, "products", "sku")? {
            conn.execute_batch("ALTER TABLE products ADD COLUMN sku TEXT;")?;
        }
        conn.execute_batch(
            "CREATE INDEX IF NOT EXISTS idx_products_ean ON products(ean) WHERE ean IS NOT NULL;",
        )?;
        Ok(())
    }

    fn column_exists(conn: &Connection, table: &str, column: &str) -> Result<bool> {
        let mut stmt = conn.prepare(&format!("PRAGMA table_info({})", table))?;
        let mut rows = stmt.query([])?;
        while let Some(row) = rows.next()? {
            if row.get::<_, String>(1)? == column {
                return Ok(true);
            }
        }
        Ok(false)
    }

    pub fn upsert_product(&self, product: &Product) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        let now = Utc::now().to_rfc3339();
        conn.execute(
            "INSERT INTO products (id, source, name, url, image_url, ean, sku, brand, first_seen, last_seen)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?9)
             ON CONFLICT(id, source) DO UPDATE SET
                name = excluded.name,
                url = excluded.url,
                image_url = COALESCE(excluded.image_url, products.image_url),
                ean = COALESCE(excluded.ean, products.ean),
                sku = COALESCE(excluded.sku, products.sku),
                brand = COALESCE(excluded.brand, products.brand),
                last_seen = excluded.last_seen",
            params![
                product.id,
                source_str(&product.source),
                product.name,
                product.url,
                product.image_url,
                product.ean,
                product.sku,
                product.brand,
                now,
            ],
        )?;
        Ok(())
    }

    /// Records that a product was seen at a price, appending to price history
    /// only when the observation actually differs from the last one.
    ///
    /// Searches run far more often than prices change, so writing a point per
    /// sighting would bury real movements under duplicates.
    pub fn record_sighting(&self, product: &Product) -> Result<bool> {
        self.upsert_product(product)?;
        if product.price_euro <= 0.0 {
            return Ok(false);
        }
        let latest = self.latest_price(&product.id, &product.source)?;
        let unchanged = latest.is_some_and(|prev| {
            (prev.price_euro - product.price_euro).abs() < 0.005 && prev.in_stock == product.in_stock
        });
        if unchanged {
            return Ok(false);
        }
        self.insert_price_point(&PricePoint {
            product_id: product.id.clone(),
            source: product.source.clone(),
            price_euro: product.price_euro,
            in_stock: product.in_stock,
            recorded_at: product.scraped_at,
        })?;
        Ok(true)
    }

    pub fn insert_price_point(&self, point: &PricePoint) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO price_history (product_id, source, price_euro, in_stock, recorded_at)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![
                point.product_id,
                source_str(&point.source),
                point.price_euro,
                point.in_stock.map(|s| s as i32),
                point.recorded_at.to_rfc3339(),
            ],
        )?;
        Ok(())
    }

    pub fn get_price_history(
        &self,
        product_id: &str,
        source: &Source,
        limit: usize,
    ) -> Result<Vec<PricePoint>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT product_id, source, price_euro, in_stock, recorded_at
             FROM price_history
             WHERE product_id = ?1 AND source = ?2
             ORDER BY recorded_at DESC
             LIMIT ?3",
        )?;
        let rows = stmt.query_map(
            params![product_id, source_str(source), limit as i64],
            price_point_from_row,
        )?;

        let mut points = Vec::new();
        for row in rows {
            points.push(row?);
        }
        Ok(points)
    }

    pub fn track_product(&self, product_id: &str, source: &Source) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT OR IGNORE INTO tracked (product_id, source, added_at) VALUES (?1, ?2, ?3)",
            params![product_id, source_str(source), Utc::now().to_rfc3339()],
        )?;
        Ok(())
    }

    pub fn untrack_product(&self, product_id: &str, source: &Source) -> Result<bool> {
        let conn = self.conn.lock().unwrap();
        let removed = conn.execute(
            "DELETE FROM tracked WHERE product_id = ?1 AND source = ?2",
            params![product_id, source_str(source)],
        )?;
        Ok(removed > 0)
    }

    pub fn list_tracked(&self) -> Result<Vec<(String, Source)>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt =
            conn.prepare("SELECT product_id, source FROM tracked ORDER BY added_at DESC")?;
        let rows = stmt.query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })?;
        let mut items = Vec::new();
        for row in rows {
            let (id, src) = row?;
            items.push((id, parse_source(&src)));
        }
        Ok(items)
    }

    pub fn search_products(&self, query: &str, source: Option<&Source>) -> Result<Vec<Product>> {
        let conn = self.conn.lock().unwrap();
        let sql = format!(
            "SELECT {} FROM products p
             WHERE (p.name LIKE '%' || ?1 || '%' OR p.id = ?1 OR p.ean = ?1 OR p.sku = ?1)
             {}
             ORDER BY p.last_seen DESC
             LIMIT 50",
            PRODUCT_COLUMNS,
            if source.is_some() { "AND p.source = ?2" } else { "" }
        );
        let mut stmt = conn.prepare(&sql)?;
        let mut products = Vec::new();
        match source {
            Some(src) => {
                let rows = stmt.query_map(params![query, source_str(src)], product_from_row)?;
                for row in rows {
                    products.push(row?);
                }
            }
            None => {
                let rows = stmt.query_map(params![query], product_from_row)?;
                for row in rows {
                    products.push(row?);
                }
            }
        }
        Ok(products)
    }

    pub fn get_product(&self, product_id: &str, source: &Source) -> Result<Option<Product>> {
        let conn = self.conn.lock().unwrap();
        let sql = format!(
            "SELECT {} FROM products p WHERE p.id = ?1 AND p.source = ?2",
            PRODUCT_COLUMNS
        );
        let mut stmt = conn.prepare(&sql)?;
        let mut rows = stmt.query_map(params![product_id, source_str(source)], product_from_row)?;
        Ok(rows.next().transpose()?)
    }

    pub fn latest_price(&self, product_id: &str, source: &Source) -> Result<Option<PricePoint>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT product_id, source, price_euro, in_stock, recorded_at
             FROM price_history
             WHERE product_id = ?1 AND source = ?2
             ORDER BY recorded_at DESC LIMIT 1",
        )?;
        let mut rows =
            stmt.query_map(params![product_id, source_str(source)], price_point_from_row)?;
        Ok(rows.next().transpose()?)
    }

    /// Records a price to watch for, and starts tracking the product so
    /// `refresh` will actually check it.
    pub fn set_alert(&self, product_id: &str, source: &Source, target_price: f64) -> Result<()> {
        self.track_product(product_id, source)?;
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO alerts (product_id, source, target_price, created_at)
             VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT(product_id, source) DO UPDATE SET target_price = excluded.target_price",
            params![
                product_id,
                source_str(source),
                target_price,
                Utc::now().to_rfc3339()
            ],
        )?;
        Ok(())
    }

    pub fn clear_alert(&self, product_id: &str, source: &Source) -> Result<bool> {
        let conn = self.conn.lock().unwrap();
        let removed = conn.execute(
            "DELETE FROM alerts WHERE product_id = ?1 AND source = ?2",
            params![product_id, source_str(source)],
        )?;
        Ok(removed > 0)
    }

    /// Lists every alert with the product's current price, marking those whose
    /// target has been reached.
    pub fn list_alerts(&self) -> Result<Vec<Alert>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT a.product_id, a.source, a.target_price, p.name, p.url,
                    (SELECT h.price_euro FROM price_history h
                      WHERE h.product_id = a.product_id AND h.source = a.source
                      ORDER BY h.recorded_at DESC LIMIT 1) AS current_price
             FROM alerts a
             LEFT JOIN products p ON p.id = a.product_id AND p.source = a.source
             ORDER BY a.created_at DESC",
        )?;
        let rows = stmt.query_map([], |row| {
            let target_price: f64 = row.get(2)?;
            let current_price: Option<f64> = row.get(5)?;
            Ok(Alert {
                product_id: row.get(0)?,
                source: parse_source(&row.get::<_, String>(1)?),
                target_price,
                name: row.get(3)?,
                url: row.get(4)?,
                current_price,
                triggered: current_price.is_some_and(|p| p > 0.0 && p <= target_price),
            })
        })?;

        let mut alerts = Vec::new();
        for row in rows {
            alerts.push(row?);
        }
        Ok(alerts)
    }

    pub fn stats(&self) -> Result<StoreStats> {
        let conn = self.conn.lock().unwrap();
        let products: usize =
            conn.query_row("SELECT COUNT(*) FROM products", [], |r| r.get::<_, i64>(0))? as usize;
        let price_points: usize = conn.query_row("SELECT COUNT(*) FROM price_history", [], |r| {
            r.get::<_, i64>(0)
        })? as usize;
        let tracked: usize =
            conn.query_row("SELECT COUNT(*) FROM tracked", [], |r| r.get::<_, i64>(0))? as usize;

        let mut stmt = conn.prepare(
            "SELECT source, COUNT(*) FROM products GROUP BY source ORDER BY COUNT(*) DESC",
        )?;
        let rows = stmt.query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)? as usize))
        })?;
        let mut by_source = Vec::new();
        for row in rows {
            let (source, count) = row?;
            by_source.push(SourceCount { source, count });
        }

        Ok(StoreStats {
            products,
            price_points,
            tracked,
            by_source,
        })
    }
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct Alert {
    pub product_id: String,
    pub source: Source,
    pub target_price: f64,
    pub name: Option<String>,
    pub url: Option<String>,
    pub current_price: Option<f64>,
    /// Whether the current price has reached the target.
    pub triggered: bool,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct SourceCount {
    pub source: String,
    pub count: usize,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct StoreStats {
    pub products: usize,
    pub price_points: usize,
    pub tracked: usize,
    pub by_source: Vec<SourceCount>,
}

pub fn source_str(source: &Source) -> &'static str {
    match source {
        Source::Jimms => "jimms",
        Source::Proshop => "proshop",
        Source::Gigantti => "gigantti",
        Source::Multitronic => "multitronic",
        Source::Datatronic => "datatronic",
        Source::Verkkokauppa => "verkkokauppa",
        Source::Power => "power",
    }
}

fn parse_source(s: &str) -> Source {
    match s {
        "proshop" => Source::Proshop,
        "gigantti" => Source::Gigantti,
        "multitronic" => Source::Multitronic,
        "datatronic" => Source::Datatronic,
        "verkkokauppa" => Source::Verkkokauppa,
        "power" => Source::Power,
        _ => Source::Jimms,
    }
}

fn price_point_from_row(row: &rusqlite::Row) -> rusqlite::Result<PricePoint> {
    Ok(PricePoint {
        product_id: row.get(0)?,
        source: parse_source(&row.get::<_, String>(1)?),
        price_euro: row.get(2)?,
        in_stock: row.get::<_, Option<i32>>(3)?.map(|v| v != 0),
        recorded_at: parse_timestamp(&row.get::<_, String>(4)?),
    })
}

fn product_from_row(row: &rusqlite::Row) -> rusqlite::Result<Product> {
    Ok(Product {
        id: row.get(0)?,
        source: parse_source(&row.get::<_, String>(1)?),
        name: row.get(2)?,
        url: row.get(3)?,
        image_url: row.get(4)?,
        ean: row.get(5)?,
        sku: row.get(6)?,
        brand: row.get(7)?,
        scraped_at: parse_timestamp(&row.get::<_, String>(8)?),
        price_euro: row.get::<_, Option<f64>>(9)?.unwrap_or(0.0),
        in_stock: row.get::<_, Option<i32>>(10)?.map(|v| v != 0),
    })
}

/// Falls back to the epoch rather than panicking on a malformed timestamp — a
/// corrupt row should not take down a whole query.
fn parse_timestamp(raw: &str) -> DateTime<Utc> {
    DateTime::parse_from_rfc3339(raw)
        .map(|t| t.with_timezone(&Utc))
        .unwrap_or_else(|_| DateTime::UNIX_EPOCH)
}

#[cfg(test)]
mod tests {
    use super::*;

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

    #[test]
    fn products_read_back_with_their_latest_price() {
        let store = Store::open_in_memory().unwrap();
        let product = sample(Source::Datatronic, "1", "AMD Ryzen 7 7800X3D", 399.0);
        store.record_sighting(&product).unwrap();

        let stored = store
            .get_product("1", &Source::Datatronic)
            .unwrap()
            .expect("product should exist");
        assert_eq!(stored.price_euro, 399.0);
        assert_eq!(stored.in_stock, Some(true));
        assert_eq!(stored.name, "AMD Ryzen 7 7800X3D");
    }

    #[test]
    fn a_repeated_sighting_at_the_same_price_adds_no_history() {
        let store = Store::open_in_memory().unwrap();
        let product = sample(Source::Datatronic, "1", "AMD Ryzen 7 7800X3D", 399.0);

        assert!(store.record_sighting(&product).unwrap());
        assert!(!store.record_sighting(&product).unwrap());
        assert!(!store.record_sighting(&product).unwrap());

        assert_eq!(store.stats().unwrap().price_points, 1);
    }

    #[test]
    fn a_price_change_appends_history() {
        let store = Store::open_in_memory().unwrap();
        let mut product = sample(Source::Datatronic, "1", "AMD Ryzen 7 7800X3D", 399.0);
        store.record_sighting(&product).unwrap();

        product.price_euro = 379.0;
        product.scraped_at = Utc::now();
        assert!(store.record_sighting(&product).unwrap());

        let history = store
            .get_price_history("1", &Source::Datatronic, 10)
            .unwrap();
        assert_eq!(history.len(), 2);
        assert_eq!(history[0].price_euro, 379.0);
        assert_eq!(
            store.get_product("1", &Source::Datatronic).unwrap().unwrap().price_euro,
            379.0
        );
    }

    #[test]
    fn a_stock_change_alone_appends_history() {
        let store = Store::open_in_memory().unwrap();
        let mut product = sample(Source::Datatronic, "1", "AMD Ryzen 7 7800X3D", 399.0);
        store.record_sighting(&product).unwrap();

        product.in_stock = Some(false);
        assert!(store.record_sighting(&product).unwrap());
        assert_eq!(store.stats().unwrap().price_points, 2);
    }

    #[test]
    fn upsert_keeps_previously_learned_identifiers() {
        let store = Store::open_in_memory().unwrap();
        let mut detailed = sample(Source::Multitronic, "3930054", "Ryzen 7 7800X3D", 359.90);
        detailed.ean = Some("0730143314930".into());
        detailed.brand = Some("AMD".into());
        store.record_sighting(&detailed).unwrap();

        let sparse = sample(Source::Multitronic, "3930054", "Ryzen 7 7800X3D", 359.90);
        store.record_sighting(&sparse).unwrap();

        let stored = store
            .get_product("3930054", &Source::Multitronic)
            .unwrap()
            .unwrap();
        assert_eq!(stored.ean.as_deref(), Some("0730143314930"));
        assert_eq!(stored.brand.as_deref(), Some("AMD"));
    }

    #[test]
    fn tracking_round_trips_and_reports_removal() {
        let store = Store::open_in_memory().unwrap();
        store.track_product("1", &Source::Datatronic).unwrap();
        store.track_product("1", &Source::Datatronic).unwrap();
        assert_eq!(store.list_tracked().unwrap().len(), 1);

        assert!(store.untrack_product("1", &Source::Datatronic).unwrap());
        assert!(!store.untrack_product("1", &Source::Datatronic).unwrap());
        assert!(store.list_tracked().unwrap().is_empty());
    }

    #[test]
    fn each_source_keeps_its_own_row_for_the_same_id() {
        let store = Store::open_in_memory().unwrap();
        store
            .record_sighting(&sample(Source::Datatronic, "1", "Product A", 10.0))
            .unwrap();
        store
            .record_sighting(&sample(Source::Jimms, "1", "Product B", 20.0))
            .unwrap();

        assert_eq!(store.stats().unwrap().products, 2);
        assert_eq!(
            store.get_product("1", &Source::Jimms).unwrap().unwrap().price_euro,
            20.0
        );
    }

    #[test]
    fn search_matches_name_id_ean_and_sku() {
        let store = Store::open_in_memory().unwrap();
        let mut product = sample(Source::Multitronic, "3930054", "AMD Ryzen 7 7800X3D", 359.90);
        product.ean = Some("0730143314930".into());
        product.sku = Some("100-100000910WOF".into());
        store.record_sighting(&product).unwrap();

        assert_eq!(store.search_products("Ryzen", None).unwrap().len(), 1);
        assert_eq!(store.search_products("3930054", None).unwrap().len(), 1);
        assert_eq!(store.search_products("0730143314930", None).unwrap().len(), 1);
        assert_eq!(store.search_products("100-100000910WOF", None).unwrap().len(), 1);
        assert_eq!(store.search_products("nothing", None).unwrap().len(), 0);
        assert_eq!(
            store.search_products("Ryzen", Some(&Source::Jimms)).unwrap().len(),
            0
        );
    }

    #[test]
    fn setting_an_alert_also_starts_tracking_the_product() {
        let store = Store::open_in_memory().unwrap();
        store
            .record_sighting(&sample(Source::Verkkokauppa, "1", "Samsung 85\" U80 TV", 899.0))
            .unwrap();

        store.set_alert("1", &Source::Verkkokauppa, 750.0).unwrap();

        assert_eq!(store.list_tracked().unwrap().len(), 1);
        let alerts = store.list_alerts().unwrap();
        assert_eq!(alerts.len(), 1);
        assert_eq!(alerts[0].target_price, 750.0);
        assert_eq!(alerts[0].current_price, Some(899.0));
        assert!(!alerts[0].triggered);
        assert_eq!(alerts[0].name.as_deref(), Some("Samsung 85\" U80 TV"));
    }

    #[test]
    fn an_alert_triggers_once_the_price_reaches_the_target() {
        let store = Store::open_in_memory().unwrap();
        let mut product = sample(Source::Verkkokauppa, "1", "Samsung 85\" U80 TV", 899.0);
        store.record_sighting(&product).unwrap();
        store.set_alert("1", &Source::Verkkokauppa, 750.0).unwrap();

        product.price_euro = 749.0;
        store.record_sighting(&product).unwrap();

        let alerts = store.list_alerts().unwrap();
        assert!(alerts[0].triggered);
        assert_eq!(alerts[0].current_price, Some(749.0));
    }

    #[test]
    fn an_alert_triggers_exactly_at_the_target_price() {
        let store = Store::open_in_memory().unwrap();
        let mut product = sample(Source::Power, "1", "TV", 900.0);
        store.record_sighting(&product).unwrap();
        store.set_alert("1", &Source::Power, 800.0).unwrap();

        product.price_euro = 800.0;
        store.record_sighting(&product).unwrap();
        assert!(store.list_alerts().unwrap()[0].triggered);
    }

    #[test]
    fn setting_an_alert_twice_updates_the_target_rather_than_duplicating() {
        let store = Store::open_in_memory().unwrap();
        store
            .record_sighting(&sample(Source::Power, "1", "TV", 900.0))
            .unwrap();
        store.set_alert("1", &Source::Power, 800.0).unwrap();
        store.set_alert("1", &Source::Power, 700.0).unwrap();

        let alerts = store.list_alerts().unwrap();
        assert_eq!(alerts.len(), 1);
        assert_eq!(alerts[0].target_price, 700.0);
    }

    #[test]
    fn clearing_an_alert_reports_whether_it_existed() {
        let store = Store::open_in_memory().unwrap();
        store.set_alert("1", &Source::Power, 800.0).unwrap();
        assert!(store.clear_alert("1", &Source::Power).unwrap());
        assert!(!store.clear_alert("1", &Source::Power).unwrap());
        assert!(store.list_alerts().unwrap().is_empty());
    }

    #[test]
    fn an_alert_on_a_product_with_no_price_yet_is_not_triggered() {
        let store = Store::open_in_memory().unwrap();
        store.set_alert("unseen", &Source::Power, 100.0).unwrap();

        let alerts = store.list_alerts().unwrap();
        assert_eq!(alerts[0].current_price, None);
        assert!(!alerts[0].triggered);
        assert_eq!(alerts[0].name, None);
    }

    #[test]
    fn stats_break_down_by_source() {
        let store = Store::open_in_memory().unwrap();
        store
            .record_sighting(&sample(Source::Datatronic, "1", "A", 10.0))
            .unwrap();
        store
            .record_sighting(&sample(Source::Datatronic, "2", "B", 20.0))
            .unwrap();
        store
            .record_sighting(&sample(Source::Jimms, "3", "C", 30.0))
            .unwrap();

        let stats = store.stats().unwrap();
        assert_eq!(stats.products, 3);
        assert_eq!(stats.by_source[0].source, "datatronic");
        assert_eq!(stats.by_source[0].count, 2);
    }

    #[test]
    fn migrating_a_legacy_database_adds_the_sku_column() {
        let dir = std::env::temp_dir().join(format!("hinta-migrate-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("legacy.db");
        let _ = std::fs::remove_file(&path);

        let legacy = Connection::open(&path).unwrap();
        legacy
            .execute_batch(
                "CREATE TABLE products (
                    id TEXT NOT NULL, source TEXT NOT NULL, name TEXT NOT NULL,
                    url TEXT NOT NULL, image_url TEXT, ean TEXT, brand TEXT,
                    first_seen TEXT NOT NULL, last_seen TEXT NOT NULL,
                    PRIMARY KEY (id, source));
                 INSERT INTO products VALUES ('1','datatronic','Legacy','u',NULL,NULL,NULL,'x','x');",
            )
            .unwrap();
        drop(legacy);

        let store = Store::open(&path).unwrap();
        let stored = store.get_product("1", &Source::Datatronic).unwrap().unwrap();
        assert_eq!(stored.name, "Legacy");
        assert_eq!(stored.sku, None);
        assert_eq!(stored.price_euro, 0.0);

        let _ = std::fs::remove_file(&path);
    }
}
