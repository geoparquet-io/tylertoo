//! Metadata retained when opening or listing a remote input object.

/// The remote input's identity, captured from the store's HEAD or listing.
///
/// Keep this separate from fetch counters: it describes the object even
/// before a reader has fetched any bytes. Last-modified is informational;
/// Entity tags are opaque and must be compared without normalization.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteObjectMeta {
    /// Object size in bytes.
    pub size: u64,
    /// Store-provided entity tag, absent for stores that do not supply one.
    pub e_tag: Option<String>,
    /// Store-provided last-modified time, as RFC 3339.
    pub last_modified: String,
}

#[cfg(feature = "remote")]
impl From<object_store::ObjectMeta> for RemoteObjectMeta {
    fn from(meta: object_store::ObjectMeta) -> Self {
        Self {
            size: meta.size,
            e_tag: meta.e_tag,
            last_modified: meta.last_modified.to_rfc3339(),
        }
    }
}
