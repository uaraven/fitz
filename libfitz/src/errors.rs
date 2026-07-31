use std::fmt::{Display, Formatter};
use fitskit::error::Error;

#[derive(Debug)]
pub enum FitsError {
    FitsError(Error),
    InvalidImageData(String),
    DecompressionError,
    ConversionError,
}

impl FitsError {
    pub fn new_invalid_img(msg: &str) -> Self {
        FitsError::InvalidImageData (msg.to_string())
    }
}

impl Display for FitsError {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            FitsError::FitsError(e) => write!(f, "FITS error: {e}"),
            FitsError::InvalidImageData(msg) => write!(f, "invalid image data: {msg}"),
            FitsError::DecompressionError => write!(f, "decompression error"),
            FitsError::ConversionError => write!(f, "conversion error"),
        }
    }
}

impl std::error::Error for FitsError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            FitsError::FitsError(e) => Some(e),
            FitsError::InvalidImageData(_) => None,
            FitsError::DecompressionError => None,
            FitsError::ConversionError => None,
        }
    }
}

impl From<Error> for FitsError {
     fn from(value: Error) -> Self {
        FitsError::FitsError(value)
    }
}
