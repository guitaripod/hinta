pub mod http;
pub mod matching;
pub mod mcp;
pub mod sources;
pub mod store;
pub mod transform;
pub mod util;

/// Directory holding the SQLite database, overridable with `HINTA_DATA_DIR`.
pub fn data_dir() -> std::path::PathBuf {
    if let Ok(dir) = std::env::var("HINTA_DATA_DIR") {
        return std::path::PathBuf::from(dir);
    }
    let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
    std::path::PathBuf::from(home).join(".local/share/hinta")
}
