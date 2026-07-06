//! Minimal test helpers for the CLI's own tests: locating bundled test data.
//! Fixture *generators* (synthesizing small FITS files) live in `fitz-core`'s
//! test support, since the domain logic that consumes them moved there.

use std::path::PathBuf;

/// Absolute path to a file under the workspace's `test-data/` directory.
pub(crate) fn test_data(filename: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("test-data")
        .join(filename)
}
