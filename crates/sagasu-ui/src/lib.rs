//! sagasu-ui: the headless backend of the M4 desktop UI.
//!
//! ## What this crate is
//!
//! One layer, sitting between `sagasu-core` and a Tauri shell:
//!
//! ```text
//! frontend (HTML/JS)          ← M4 step 3
//!     ↕ JSON, over the webview boundary
//! src-tauri  #[tauri::command] ← M4 step 2, a thin wrapper
//!     ↕ Rust
//! sagasu-ui  ← this crate: DTOs + command functions
//!     ↕ Rust
//! sagasu-core  browse::browse(&Store, &BrowseQuery) -> BrowseView
//! ```
//!
//! ## Why it has no `tauri` dependency
//!
//! Because nothing here needs one. [`browse_view`] is a plain function over
//! plain data; step 2 wraps it in a `#[tauri::command]` that adds a `State`
//! extractor and nothing else. Keeping the dependency out buys two things:
//!
//! - **it builds and tests anywhere.** The Tauri shell needs webkit2gtk system
//!   libraries; this crate needs `cargo test`. So the layer where the DTOs, the
//!   conversions and the existence check actually live is covered by CI on
//!   every platform, and the part that cannot be is reduced to a wrapper with
//!   no logic in it.
//! - **the boundary stays inspectable.** A test can call [`browse_view`] and
//!   assert on the JSON a webview would receive, without a webview.
//!
//! ## Why it exists rather than the shell calling the core directly
//!
//! `docs/design.md` §3 has M4 linking `sagasu-core` directly, and it does —
//! through here. Two things have to happen between `browse()` and a webview,
//! and neither belongs in the core:
//!
//! 1. **Serialization.** `docs/cli.md` §8: the core types carry no `Serialize`,
//!    on purpose. A Tauri command's return value is serialized by serde, so
//!    something has to be serializable, and that something is [`dto`].
//! 2. **The existence check.** `docs/browse.md` §6: `browse()` touches nothing
//!    but the database, and checking whether the previewed paths still exist is
//!    the caller's job. In the terminal, `sagasu-cli` is that caller. In the UI
//!    stack, this crate is — see [`dto::BrowseViewDto::from_view`].
//!
//! ## Scope
//!
//! Browse only. `find` / `search` get the same treatment in a later slice; when
//! they do, they arrive as more DTOs and more command functions in exactly this
//! shape, not as a second pattern.

pub mod dto;

use anyhow::Result;

use sagasu_core::browse;
use sagasu_core::store::Store;

pub use dto::{
    BrowseQueryDto, BrowseViewDto, FacetAxisDto, FacetValueDto, FileRowDto, LabelTermDto,
    NextStepDto, TagDto, TagLayerSnapshotDto,
};

/// One drill-down step, as the UI asks for it.
///
/// The whole of the command layer for `browse`: parse the query the webview
/// sent, hand it to [`sagasu_core::browse::browse`], and convert the answer for
/// the trip back — existence-checking the previewed rows on the way, because
/// that is this layer's job (see the crate docs).
///
/// Deliberately shaped so step 2 is a wrapper and not a reimplementation:
///
/// ```ignore
/// #[tauri::command]
/// fn browse_view(state: tauri::State<AppState>, query: BrowseQueryDto)
///     -> Result<BrowseViewDto, String>
/// {
///     sagasu_ui::browse_view(&state.store.lock().unwrap(), &query)
///         .map_err(|e| e.to_string())
/// }
/// ```
///
/// Errors are `anyhow` and come from two places only: a malformed tag in the
/// query, and the database.
pub fn browse_view(store: &Store, query: &BrowseQueryDto) -> Result<BrowseViewDto> {
    let view = browse::browse(store, &query.to_core()?)?;
    Ok(BrowseViewDto::from_view(view))
}
