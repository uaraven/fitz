use std::path::PathBuf;

use fitskit::CompressionType;

pub struct Options {
    pub keep: bool,
    pub force: bool,
    pub verbose: bool,
    pub output: Option<PathBuf>,
    pub algorithm: CompressionType,
    pub multi_file: bool,
}

impl Default for Options {
    fn default() -> Self {
        Options {
            keep: false,
            force: false,
            verbose: false,
            output: None,
            algorithm: CompressionType::Rice1,
            multi_file: false,
        }
    }
}
