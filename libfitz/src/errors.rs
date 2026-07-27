use std::fmt::{Display, Formatter};
use fitskit::error::Error;

#[derive(Debug)]
pub enum FitsError {
    FitsError(Error),
    InvalidImageData(String),
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
        }
    }
}

impl From<Error> for FitsError {
     fn from(value: Error) -> Self {
        FitsError::FitsError(value)
    }
}