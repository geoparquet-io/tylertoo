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
impl RemoteObjectMeta {
    /// Fetch bytes bound to the object identity captured by HEAD or listing.
    ///
    /// Both footer/data reads and coalesced staging use this precondition so
    /// replacing an object cannot mix bytes from different versions (#547).
    /// Stores without entity tags retain the existing size/footer identity contract.
    ///
    /// # Errors
    /// Returns the store's precondition error if the ETag changed, or any
    /// transport/body error. A failed conditional read is never retried unbound.
    pub async fn get_range(
        &self,
        store: &dyn object_store::ObjectStore,
        location: &object_store::path::Path,
        range: std::ops::Range<u64>,
    ) -> object_store::Result<bytes::Bytes> {
        let options = object_store::GetOptions {
            if_match: self.e_tag.clone(),
            range: Some(range.into()),
            ..Default::default()
        };
        store.get_opts(location, options).await?.bytes().await
    }
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
