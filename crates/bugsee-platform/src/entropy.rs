//! Random bytes for IDs / nonces.

use crate::error::EntropyError;

pub trait Entropy: Send + Sync {
    fn fill(&self, buf: &mut [u8]) -> Result<(), EntropyError>;
}

/// OS entropy via `getrandom` (desktop default).
#[derive(Debug, Default, Clone, Copy)]
pub struct SystemEntropy;

impl Entropy for SystemEntropy {
    fn fill(&self, buf: &mut [u8]) -> Result<(), EntropyError> {
        getrandom::getrandom(buf).map_err(|e| EntropyError::Unavailable(e.to_string()))
    }
}
