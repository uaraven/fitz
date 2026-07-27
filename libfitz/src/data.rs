

/// A pixel buffer can be one of three types: u8, u16, or f32. Each type is represented by a vector of the corresponding type.
pub enum PixelBuffer {
    U16(Vec<u16>),
    F32(Vec<f32>),
}

/// A Pixels struct contains a pixel buffer, width, and height. The pixel buffer can be one of three types: u8, u16, or f32.
pub struct Pixels {
    pub data: PixelBuffer,
    pub width: usize,
    pub height: usize,
}

/// An ImageType enum represents the type of an image, which can be either RGB or CFA (Color Filter Array) - unbayered.
pub enum ImageType {
    RGB,
    Grayscale,
    CFA,
}

/// An Image struct contains an image type, width, height, and a vector of pixel buffers. The pixel buffers can be one of three types: u8, u16, or f32.
pub struct Image {
    pub image_type: ImageType,
    pub width: usize,
    pub height: usize,

    pub channels: Vec<PixelBuffer>,
}

impl Image {
    /// Creates a new Image with the specified image type, width, height, and pixel buffers.
    pub fn new(image_type: ImageType, width: usize, height: usize, channels: Vec<PixelBuffer>) -> Self {
        Self {
            image_type,
            width,
            height,
            channels,
        }
    }
}
