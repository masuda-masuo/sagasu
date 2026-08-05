//! proto-gui: thin Tauri v2 measurement-instrument shell.
//!
//! ALL search/merge/stats logic lives in proto-gui-core.  This crate is a
//! wrapper that exposes a Tauri command API and a static HTML frontend.
//! It is ONLY compiled on Windows (CI) — it cannot build on Linux because
//! of the webkit2gtk system-library requirement.

use std::path::PathBuf;
use std::sync::Mutex;

use anyhow::Result;
use clap::Parser;
use proto_gui_core::{Engine, JsonlRecord, SearchMode};

/// Tauri v2 managed state: the engine behind a Mutex, plus an in-memory
/// JSONL buffer for export.
struct AppState {
    engine: Mutex<Engine>,
    /// In-memory log: one entry per search call.  Client timings (ipc_ms,
    /// render_ms) are patched in later via `record_client_timing`.
    log: Mutex<Vec<JsonlRecord>>,
}

// ---------------------------------------------------------------------------
// CLI
// ---------------------------------------------------------------------------

#[derive(Parser)]
#[command(about = "sagasu proto-gui — M2 incremental search measurement instrument")]
struct Args {
    /// Path to the tantivy index directory (must contain sagasu-meta.txt).
    #[arg(long)]
    index_dir: PathBuf,
}

// ---------------------------------------------------------------------------
// Tauri commands
// ---------------------------------------------------------------------------

#[tauri::command]
fn search(
    state: tauri::State<'_, AppState>,
    query: String,
    limit: usize,
    mode: String,
    ttl_ms: Option<u64>,
    max_size: Option<u64>,
) -> Result<serde_json::Value, String> {
    let search_mode = match mode.as_str() {
        "IndexOnly" => SearchMode::IndexOnly,
        "DeltaEveryQuery" => SearchMode::DeltaEveryQuery,
        "DeltaCached" => SearchMode::DeltaCached {
            ttl_ms: ttl_ms.unwrap_or(5000),
        },
        other => return Err(format!("unknown mode: {other}")),
    };

    let max_size = max_size.unwrap_or(2 * 1024 * 1024);

    let mut engine = state.engine.lock().map_err(|e| e.to_string())?;
    let resp = engine
        .search(&query, limit, &search_mode, max_size)
        .map_err(|e| e.to_string())?;

    // Record log entry (client timings will be patched later).
    let mode_str = match &search_mode {
        SearchMode::IndexOnly => "IndexOnly".to_string(),
        SearchMode::DeltaEveryQuery => "DeltaEveryQuery".to_string(),
        SearchMode::DeltaCached { ttl_ms } => {
            format!("DeltaCached(ttl={ttl_ms}ms)")
        }
    };

    let record_id = {
        let mut log = state.log.lock().map_err(|e| e.to_string())?;
        log.push(JsonlRecord {
            timestamp_unix_s: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs_f64())
                .unwrap_or(0.0),
            query: query.clone(),
            mode: mode_str,
            limit,
            timings: resp.timings.clone(),
            delta_changed_count: resp.delta_changed_count,
            delta_cache_hit: resp.delta_cache_hit,
            delta_cache_age_ms: resp.delta_cache_age_ms,
            total_hits: resp.total_hits_before_limit,
        });
        log.len() - 1
    };

    // record_id lets the client patch ITS OWN log entry with ipc/render
    // timings — patching "the last entry" would corrupt records when a
    // later keystroke's search lands before the previous patch arrives.
    let mut value = serde_json::to_value(&resp).map_err(|e| e.to_string())?;
    value["record_id"] = serde_json::json!(record_id);
    Ok(value)
}

/// Patch a specific log entry with client-measured timings.
#[tauri::command]
fn record_client_timing(
    state: tauri::State<'_, AppState>,
    record_id: usize,
    ipc_ms: f64,
    render_ms: f64,
) -> Result<(), String> {
    let mut log = state.log.lock().map_err(|e| e.to_string())?;
    if let Some(rec) = log.get_mut(record_id) {
        rec.timings.ipc_ms = ipc_ms;
        rec.timings.render_ms = render_ms;
    }
    Ok(())
}

#[tauri::command]
fn refresh_delta(state: tauri::State<'_, AppState>) -> Result<(), String> {
    let mut engine = state.engine.lock().map_err(|e| e.to_string())?;
    engine.refresh_delta();
    Ok(())
}

#[tauri::command]
fn export_log(state: tauri::State<'_, AppState>, path: String) -> Result<(), String> {
    let log = state.log.lock().map_err(|e| e.to_string())?;
    let mut out = String::new();
    for rec in log.iter() {
        let line = serde_json::to_string(rec).map_err(|e| e.to_string())?;
        out.push_str(&line);
        out.push('\n');
    }
    std::fs::write(&path, out).map_err(|e| format!("failed to write {path}: {e}"))?;
    Ok(())
}

#[tauri::command]
fn get_meta(state: tauri::State<'_, AppState>) -> Result<serde_json::Value, String> {
    let engine = state.engine.lock().map_err(|e| e.to_string())?;
    Ok(serde_json::json!({
        "root": engine.root().to_string_lossy(),
        "marker_ns": engine.marker_ns(),
    }))
}

// ---------------------------------------------------------------------------
// Entry point
// ---------------------------------------------------------------------------

fn main() -> Result<()> {
    let args = Args::parse();

    // Open the index once — the fixed cost of a long-lived process.
    let engine = Engine::open(&args.index_dir)
        .map_err(|e| anyhow::anyhow!("failed to open index at {}: {e}", args.index_dir.display()))?;

    let state = AppState {
        engine: Mutex::new(engine),
        log: Mutex::new(Vec::new()),
    };

    tauri::Builder::default()
        .manage(state)
        .invoke_handler(tauri::generate_handler![
            search,
            record_client_timing,
            refresh_delta,
            export_log,
            get_meta,
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");

    Ok(())
}
