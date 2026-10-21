use thiserror::Error;

/// Errors raised by the cartogram computation and the I/O layer.
#[derive(Debug, Error)]
pub enum Error {
    #[error("attribute '{0}' not found")]
    AttributeNotFound(String),

    #[error("attribute '{0}' has no numeric value")]
    AttributeNotNumeric(String),

    /// A cartogram needs some mass to redistribute; ScapeToad raised the same error.
    #[error("all values of attribute '{0}' are zero or negative")]
    AllValuesZero(String),

    #[error("the master layer has no polygon with a positive area")]
    NoPolygonArea,

    /// A NaN or infinite coordinate would turn every density, and so the whole field, to NaN.
    #[error("feature {feature} of layer '{layer}' has a non-finite coordinate")]
    NonFiniteCoordinate { layer: String, feature: usize },

    #[error("the cartogram grid must have at least 3x3 nodes, got {0}x{1}")]
    GridTooSmall(usize, usize),

    /// A [`crate::CartogramOptions`] value outside its range.
    #[error("invalid option: {0}")]
    InvalidOption(String),

    #[error("invalid deformation field: {0}")]
    InvalidField(String),

    #[error("unsupported format '{0}'")]
    UnsupportedFormat(String),

    #[error("{format}: {message}")]
    Format {
        format: &'static str,
        message: String,
    },

    #[error(transparent)]
    Io(#[from] std::io::Error),
}

pub type Result<T, E = Error> = std::result::Result<T, E>;

impl Error {
    /// Wraps an error from a format backend, keeping only its message.
    pub(crate) fn format(format: &'static str, err: impl std::fmt::Display) -> Self {
        Error::Format {
            format,
            message: err.to_string(),
        }
    }
}
