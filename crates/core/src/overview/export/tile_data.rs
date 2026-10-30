//! The current tile bytes and the gzip derived from exactly those bytes.
use std::io;
use std::ops::{Deref, DerefMut};

use crate::compression::TileGzipCache;

#[derive(Debug)]
pub(super) struct TileData {
    bytes: Vec<u8>,
    // Read-only through TileMvt's Deref view, including F's cost observer.
    pub(super) gzip: Option<Vec<u8>>,
}

impl From<Vec<u8>> for TileData {
    fn from(bytes: Vec<u8>) -> Self {
        Self { bytes, gzip: None }
    }
}

impl TileData {
    pub(super) fn gzipped(&mut self, cache: &TileGzipCache) -> io::Result<&[u8]> {
        if self.gzip.is_none() {
            self.gzip = Some(cache.compress(&self.bytes)?);
        }
        Ok(self.gzip.as_deref().expect("filled above"))
    }

    pub(super) fn into_gzip(self, cache: &TileGzipCache) -> io::Result<Vec<u8>> {
        match self.gzip {
            Some(gzip) => Ok(gzip),
            None => cache.compress(&self.bytes),
        }
    }

    #[cfg(test)]
    pub(super) fn into_raw(self) -> Vec<u8> {
        self.bytes
    }
}

impl Deref for TileData {
    type Target = Vec<u8>;
    fn deref(&self) -> &Self::Target {
        &self.bytes
    }
}

impl DerefMut for TileData {
    fn deref_mut(&mut self) -> &mut Self::Target {
        // No mutable access to the raw Vec escapes without invalidating its
        // gzip first, including same-length, in-place edits.
        self.gzip = None;
        &mut self.bytes
    }
}

impl PartialEq for TileData {
    fn eq(&self, other: &Self) -> bool {
        self.bytes == other.bytes
    }
}
impl PartialEq<Vec<u8>> for TileData {
    fn eq(&self, other: &Vec<u8>) -> bool {
        self.bytes == *other
    }
}
