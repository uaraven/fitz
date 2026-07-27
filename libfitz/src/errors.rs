
use fitskit::error::Error;

#[derive(Debug)]
pub enum FitsError {
    FitsError(fitskit::error::Error),
    InvalidImageData(String),
}

impl FitsError {
    pub fn new_invalid_img(msg: &str) -> Self {
        FitsError::InvalidImageData (msg.to_string())
    }
}

impl From<Error> for FitsError {
     fn from(value: Error) -> Self {
        FitsError::FitsError(value)
    }
}