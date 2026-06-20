//! Library half of `ghostframe-test-pattern`.
//!
//! Re-exports the modules the e2e tests need to import (font tables, text-grid
//! coordinates). The binary CLI lives in `main.rs`.

pub mod drm_direct;
pub mod font;
pub mod gradient;
pub mod lossless_golden;
pub mod mixed;
pub mod mode_switch;
pub mod palette_churn;
pub mod palrle_exact;
pub mod solid_per_tile;
pub mod subtle_drift;
pub mod text_grid;
pub mod text_grid_drm;
pub mod tile_pattern;
