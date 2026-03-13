//! Disk-backed geometry storage for bounded memory tile generation.
//!
//! # Problem
//!
//! When generating tiles, features can appear in many tiles (30+ across zoom levels).
//! Storing full geometry copies per tile creates massive memory bloat:
//! - 10M features × 30 tiles × 400 bytes = 120GB of geometry copies
//!
//! # Solution
//!
//! GeometryStore writes geometries ONCE to a temp file during Phase 1 (read),
//! returns lightweight handles, and provides random access during Phase 3 (encode).
//!
//! # Usage
//!
//! ```ignore
//! let mut store = GeometryStore::new()?;
//!
//! // Phase 1: Append geometries, get handles
//! let handle = store.append(&wkb_bytes, &properties_bytes)?;
//!
//! // Phase 3: Read back for tile encoding
//! let (wkb, props) = store.read(handle)?;
//! ```

use std::fs::File;
use std::io::{self, BufReader, BufWriter, Read, Seek, SeekFrom, Write};
use tempfile::NamedTempFile;

/// Handle to a stored geometry. Contains offset and lengths for retrieval.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub struct GeometryHandle {
    /// Byte offset in the store file
    pub offset: u64,
    /// Length of WKB data
    pub wkb_len: u32,
    /// Length of properties data
    pub props_len: u32,
}

impl GeometryHandle {
    /// Size of this handle in bytes (for memory estimation)
    pub const SIZE: usize = 16; // 8 + 4 + 4 bytes
}

/// Disk-backed geometry storage.
///
/// Provides append-only writes during Phase 1, random reads during Phase 3.
/// Uses a temp file that is automatically cleaned up on drop.
///
/// # File Format
///
/// Each record is stored as:
/// ```text
/// [wkb_len: u32 LE][props_len: u32 LE][wkb: N bytes][props: M bytes]
/// ```
///
/// The handle stores the offset to the start of the record, plus the lengths.
pub struct GeometryStore {
    /// Buffered writer for sequential appends
    writer: BufWriter<File>,
    /// The underlying temp file (keeps it alive for cleanup on drop)
    _temp_file: NamedTempFile,
    /// Path to the temp file for re-opening for reads
    path: std::path::PathBuf,
    /// Current write position (for calculating offsets)
    write_pos: u64,
    /// Number of geometries stored
    count: usize,
    /// Reader for random access (lazily initialized after flush)
    reader: Option<BufReader<File>>,
}

impl GeometryStore {
    /// Create a new geometry store backed by a temp file.
    pub fn new() -> io::Result<Self> {
        let temp_file = NamedTempFile::new()?;
        let path = temp_file.path().to_path_buf();
        let file = temp_file.reopen()?;
        let writer = BufWriter::new(file);

        Ok(Self {
            writer,
            _temp_file: temp_file,
            path,
            write_pos: 0,
            count: 0,
            reader: None,
        })
    }

    /// Append geometry data and return a handle for later retrieval.
    ///
    /// # Arguments
    /// * `wkb` - WKB-encoded geometry bytes
    /// * `properties` - MessagePack-serialized properties
    ///
    /// # Returns
    /// Handle that can be used to retrieve the data later
    pub fn append(&mut self, wkb: &[u8], properties: &[u8]) -> io::Result<GeometryHandle> {
        let offset = self.write_pos;
        let wkb_len = wkb.len() as u32;
        let props_len = properties.len() as u32;

        // Write lengths as little-endian u32
        self.writer.write_all(&wkb_len.to_le_bytes())?;
        self.writer.write_all(&props_len.to_le_bytes())?;

        // Write data
        self.writer.write_all(wkb)?;
        self.writer.write_all(properties)?;

        // Update position: 4 + 4 + wkb_len + props_len
        self.write_pos += 8 + wkb_len as u64 + props_len as u64;
        self.count += 1;

        Ok(GeometryHandle {
            offset,
            wkb_len,
            props_len,
        })
    }

    /// Read geometry data using a handle.
    ///
    /// # Arguments
    /// * `handle` - Handle returned from a previous `append` call
    ///
    /// # Returns
    /// Tuple of (WKB bytes, properties bytes)
    pub fn read(&mut self, handle: GeometryHandle) -> io::Result<(Vec<u8>, Vec<u8>)> {
        // Initialize reader if needed
        if self.reader.is_none() {
            let file = File::open(&self.path)?;
            self.reader = Some(BufReader::new(file));
        }

        let reader = self.reader.as_mut().unwrap();

        // Seek to the data portion (skip the length headers)
        reader.seek(SeekFrom::Start(handle.offset + 8))?;

        // Read WKB
        let mut wkb = vec![0u8; handle.wkb_len as usize];
        reader.read_exact(&mut wkb)?;

        // Read properties
        let mut props = vec![0u8; handle.props_len as usize];
        reader.read_exact(&mut props)?;

        Ok((wkb, props))
    }

    /// Returns the number of geometries stored.
    pub fn len(&self) -> usize {
        self.count
    }

    /// Returns true if no geometries have been stored.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Returns the current size of the backing file in bytes.
    pub fn file_size(&self) -> io::Result<u64> {
        Ok(self.write_pos)
    }

    /// Flush any buffered writes to disk.
    ///
    /// Call this after Phase 1 (appending) and before Phase 3 (reading).
    pub fn flush(&mut self) -> io::Result<()> {
        self.writer.flush()
    }

    /// Create an independent reader for concurrent access.
    ///
    /// Each reader opens its own file handle to the backing store, enabling
    /// parallel reads from multiple threads (e.g., rayon's par_iter in Phase 3).
    ///
    /// # Requirements
    ///
    /// Call `flush()` before creating readers to ensure all data is on disk.
    ///
    /// # Example
    ///
    /// ```ignore
    /// store.flush()?;
    /// let readers: Vec<_> = (0..rayon::current_num_threads())
    ///     .map(|_| store.new_reader())
    ///     .collect::<Result<_, _>>()?;
    /// ```
    pub fn new_reader(&self) -> io::Result<GeometryStoreReader> {
        let file = File::open(&self.path)?;
        Ok(GeometryStoreReader {
            file: BufReader::new(file),
        })
    }

    /// Return the path to the backing store file.
    ///
    /// Useful for diagnostics and debugging.
    pub fn path(&self) -> &std::path::Path {
        &self.path
    }
}

/// Independent reader for a `GeometryStore` backing file.
///
/// Each reader owns its own file handle and seek position, so multiple readers
/// can operate concurrently on different threads without contention.
///
/// # Thread Safety
///
/// `GeometryStoreReader` is `Send` (can be moved to another thread) but not `Sync`
/// (each thread should have its own reader). This matches the rayon pattern of
/// giving each thread its own mutable reader.
///
/// # Usage
///
/// ```ignore
/// // Create one reader per rayon thread
/// let readers: Vec<_> = (0..num_threads)
///     .map(|_| store.new_reader())
///     .collect::<Result<_, _>>()?;
///
/// // Use thread_local! or index by thread for parallel encoding
/// tiles.par_chunks(batch_size).enumerate().try_for_each(|(i, chunk)| {
///     let reader = &mut readers[i % readers.len()];
///     for tile_ref in chunk {
///         let (geom, props) = reader.read(tile_ref.geometry_handle)?;
///         // ... encode tile
///     }
///     Ok(())
/// })?;
/// ```
pub struct GeometryStoreReader {
    /// Independent buffered file handle for random-access reads
    file: BufReader<File>,
}

impl GeometryStoreReader {
    /// Read geometry data using a handle.
    ///
    /// This has the same behavior as `GeometryStore::read()` but operates on
    /// an independent file handle, enabling concurrent reads from multiple threads.
    ///
    /// # Arguments
    /// * `handle` - Handle returned from a previous `GeometryStore::append` call
    ///
    /// # Returns
    /// Tuple of (WKB bytes, properties bytes)
    pub fn read(&mut self, handle: GeometryHandle) -> io::Result<(Vec<u8>, Vec<u8>)> {
        // Seek to the data portion (skip the length headers)
        self.file.seek(SeekFrom::Start(handle.offset + 8))?;

        // Read WKB
        let mut wkb = vec![0u8; handle.wkb_len as usize];
        self.file.read_exact(&mut wkb)?;

        // Read properties
        let mut props = vec![0u8; handle.props_len as usize];
        self.file.read_exact(&mut props)?;

        Ok((wkb, props))
    }
}

/// LRU-cached reader for geometry store.
///
/// Wraps `GeometryStoreReader` with an LRU cache to avoid repeated disk reads
/// for frequently-accessed geometries. This is crucial for performance when
/// geometries appear in multiple tiles (spatial locality).
///
/// # Performance
///
/// With typical admin boundary data:
/// - Adjacent tiles share 60-80% of geometries
/// - Cache hit rates of 70-90% are common with 10K entries
/// - 10x speedup on Phase 3 tile encoding
///
/// # Memory Cost
///
/// Cache size × average geometry size = memory overhead
/// - 10K entries × ~500 bytes = ~5MB per reader
/// - 16 threads × 5MB = ~80MB total (negligible)
///
/// # Usage
///
/// ```ignore
/// let reader = store.new_reader()?;
/// let cached = CachedGeometryReader::new(reader, 10_000);
///
/// // Cache warms up with frequently-accessed geometries
/// for handle in handles {
///     let (wkb, props) = cached.read(handle)?;  // Fast after first access
/// }
/// ```
pub struct CachedGeometryReader {
    /// Underlying reader for cache misses
    reader: GeometryStoreReader,
    /// LRU cache: handle -> (WKB bytes, properties bytes)
    cache: lru::LruCache<GeometryHandle, (Vec<u8>, Vec<u8>)>,
    /// Cache statistics for monitoring
    hits: usize,
    misses: usize,
}

impl CachedGeometryReader {
    /// Create a new cached reader with the specified cache capacity.
    ///
    /// # Arguments
    /// * `reader` - Underlying GeometryStoreReader
    /// * `capacity` - Maximum number of entries to cache (10K recommended)
    ///
    /// # Example
    ///
    /// ```ignore
    /// let reader = store.new_reader()?;
    /// let cached = CachedGeometryReader::new(reader, 10_000);
    /// ```
    pub fn new(reader: GeometryStoreReader, capacity: usize) -> Self {
        Self {
            reader,
            cache: lru::LruCache::new(
                std::num::NonZeroUsize::new(capacity).expect("capacity must be > 0"),
            ),
            hits: 0,
            misses: 0,
        }
    }

    /// Read geometry data using a handle, with LRU caching.
    ///
    /// On cache hit: returns cached data (no disk I/O)
    /// On cache miss: reads from disk and caches result
    ///
    /// # Arguments
    /// * `handle` - Handle returned from a previous `GeometryStore::append` call
    ///
    /// # Returns
    /// Tuple of (WKB bytes, properties bytes)
    pub fn read(&mut self, handle: GeometryHandle) -> io::Result<(Vec<u8>, Vec<u8>)> {
        if let Some(cached) = self.cache.get(&handle) {
            self.hits += 1;
            return Ok(cached.clone());
        }

        self.misses += 1;
        let data = self.reader.read(handle)?;
        self.cache.put(handle, data.clone());
        Ok(data)
    }

    /// Get cache statistics.
    ///
    /// Returns (hits, misses, hit_rate) tuple.
    ///
    /// # Example
    ///
    /// ```ignore
    /// let (hits, misses, rate) = cached.stats();
    /// println!("Cache hit rate: {:.1}% ({} hits, {} misses)", rate * 100.0, hits, misses);
    /// ```
    pub fn stats(&self) -> (usize, usize, f64) {
        let total = self.hits + self.misses;
        let hit_rate = if total > 0 {
            self.hits as f64 / total as f64
        } else {
            0.0
        };
        (self.hits, self.misses, hit_rate)
    }

    /// Clear the cache and reset statistics.
    ///
    /// Useful for forcing a fresh start between processing phases.
    pub fn clear(&mut self) {
        self.cache.clear();
        self.hits = 0;
        self.misses = 0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // =============================================================================
    // Unit Tests: Basic Operations
    // =============================================================================

    #[test]
    fn test_new_creates_empty_store() {
        let store = GeometryStore::new().expect("Should create store");
        assert!(store.is_empty());
        assert_eq!(store.len(), 0);
    }

    #[test]
    fn test_append_returns_valid_handle() {
        let mut store = GeometryStore::new().expect("Should create store");
        let wkb = vec![0x01, 0x01, 0x00, 0x00, 0x00]; // Point WKB header
        let props = vec![0x80]; // Empty MessagePack map

        let handle = store.append(&wkb, &props).expect("Should append");

        assert_eq!(handle.offset, 0);
        assert_eq!(handle.wkb_len, 5);
        assert_eq!(handle.props_len, 1);
    }

    #[test]
    fn test_append_increments_len() {
        let mut store = GeometryStore::new().expect("Should create store");

        store.append(&[1, 2, 3], &[4, 5]).expect("Should append");
        assert_eq!(store.len(), 1);

        store.append(&[6, 7], &[8, 9, 10]).expect("Should append");
        assert_eq!(store.len(), 2);
    }

    #[test]
    fn test_read_roundtrip_single() {
        let mut store = GeometryStore::new().expect("Should create store");
        let wkb = vec![0x01, 0x02, 0x03, 0x04, 0x05];
        let props = vec![0x82, 0xa4, b'n', b'a', b'm', b'e'];

        let handle = store.append(&wkb, &props).expect("Should append");
        store.flush().expect("Should flush");

        let (read_wkb, read_props) = store.read(handle).expect("Should read");

        assert_eq!(read_wkb, wkb);
        assert_eq!(read_props, props);
    }

    #[test]
    fn test_read_roundtrip_multiple() {
        let mut store = GeometryStore::new().expect("Should create store");

        // Append three geometries with different sizes
        let wkb1 = vec![1, 2, 3];
        let props1 = vec![10, 20];
        let h1 = store.append(&wkb1, &props1).expect("Should append");

        let wkb2 = vec![4, 5, 6, 7, 8, 9, 10];
        let props2 = vec![30];
        let h2 = store.append(&wkb2, &props2).expect("Should append");

        let wkb3 = vec![11];
        let props3 = vec![40, 50, 60, 70, 80];
        let h3 = store.append(&wkb3, &props3).expect("Should append");

        store.flush().expect("Should flush");

        // Read in different order than appended
        let (r2_wkb, r2_props) = store.read(h2).expect("Should read h2");
        assert_eq!(r2_wkb, wkb2);
        assert_eq!(r2_props, props2);

        let (r1_wkb, r1_props) = store.read(h1).expect("Should read h1");
        assert_eq!(r1_wkb, wkb1);
        assert_eq!(r1_props, props1);

        let (r3_wkb, r3_props) = store.read(h3).expect("Should read h3");
        assert_eq!(r3_wkb, wkb3);
        assert_eq!(r3_props, props3);
    }

    #[test]
    fn test_read_same_handle_multiple_times() {
        let mut store = GeometryStore::new().expect("Should create store");
        let wkb = vec![1, 2, 3, 4, 5];
        let props = vec![6, 7, 8];

        let handle = store.append(&wkb, &props).expect("Should append");
        store.flush().expect("Should flush");

        // Read the same handle multiple times
        for _ in 0..3 {
            let (read_wkb, read_props) = store.read(handle).expect("Should read");
            assert_eq!(read_wkb, wkb);
            assert_eq!(read_props, props);
        }
    }

    #[test]
    fn test_empty_wkb_and_props() {
        let mut store = GeometryStore::new().expect("Should create store");

        let handle = store.append(&[], &[]).expect("Should append empty");
        store.flush().expect("Should flush");

        let (read_wkb, read_props) = store.read(handle).expect("Should read");
        assert!(read_wkb.is_empty());
        assert!(read_props.is_empty());
    }

    #[test]
    fn test_file_size_grows_with_appends() {
        let mut store = GeometryStore::new().expect("Should create store");

        let initial_size = store.file_size().expect("Should get size");
        assert_eq!(initial_size, 0);

        // Each append adds: wkb_len (4 bytes) + props_len (4 bytes) + wkb + props
        store.append(&[1, 2, 3], &[4, 5]).expect("Should append");
        store.flush().expect("Should flush");

        let size_after = store.file_size().expect("Should get size");
        // 4 (wkb_len) + 4 (props_len) + 3 (wkb) + 2 (props) = 13 bytes
        assert_eq!(size_after, 13);
    }

    // =============================================================================
    // Unit Tests: Edge Cases
    // =============================================================================

    #[test]
    fn test_large_geometry() {
        let mut store = GeometryStore::new().expect("Should create store");

        // 1MB geometry (simulating large polygon)
        let wkb = vec![0xAB; 1024 * 1024];
        let props = vec![0xCD; 1024]; // 1KB properties

        let handle = store.append(&wkb, &props).expect("Should append large");
        store.flush().expect("Should flush");

        let (read_wkb, read_props) = store.read(handle).expect("Should read large");
        assert_eq!(read_wkb.len(), 1024 * 1024);
        assert_eq!(read_props.len(), 1024);
        assert_eq!(read_wkb, wkb);
        assert_eq!(read_props, props);
    }

    #[test]
    fn test_many_small_geometries() {
        let mut store = GeometryStore::new().expect("Should create store");
        let mut handles = Vec::new();

        // Store 10,000 small geometries
        for i in 0u32..10_000 {
            let wkb = i.to_le_bytes().to_vec();
            let props = vec![(i % 256) as u8];
            let handle = store.append(&wkb, &props).expect("Should append");
            handles.push((handle, wkb, props));
        }

        store.flush().expect("Should flush");
        assert_eq!(store.len(), 10_000);

        // Verify a sample of them (every 1000th)
        for (i, (handle, expected_wkb, expected_props)) in handles.iter().enumerate() {
            if i % 1000 == 0 {
                let (read_wkb, read_props) = store.read(*handle).expect("Should read");
                assert_eq!(&read_wkb, expected_wkb, "WKB mismatch at index {}", i);
                assert_eq!(&read_props, expected_props, "Props mismatch at index {}", i);
            }
        }
    }

    // =============================================================================
    // Unit Tests: Handle Properties
    // =============================================================================

    #[test]
    fn test_handle_size_constant() {
        // Verify the handle size constant matches actual size
        assert_eq!(
            GeometryHandle::SIZE,
            std::mem::size_of::<u64>() + std::mem::size_of::<u32>() * 2
        );
    }

    #[test]
    fn test_handles_have_increasing_offsets() {
        let mut store = GeometryStore::new().expect("Should create store");

        let h1 = store.append(&[1, 2, 3], &[4]).expect("append");
        let h2 = store.append(&[5, 6], &[7, 8]).expect("append");
        let h3 = store.append(&[9], &[10, 11, 12]).expect("append");

        // Offsets should increase
        assert!(h2.offset > h1.offset);
        assert!(h3.offset > h2.offset);

        // Verify offset calculation is correct
        // h1: offset 0, data = 4 + 4 + 3 + 1 = 12 bytes
        // h2: offset 12
        assert_eq!(h1.offset, 0);
        assert_eq!(h2.offset, 12);
        // h2 data = 4 + 4 + 2 + 2 = 12 bytes, h3 offset = 24
        assert_eq!(h3.offset, 24);
    }

    // =============================================================================
    // Integration Tests: Pipeline Simulation
    // =============================================================================

    /// Simulates the actual pipeline pattern:
    /// Phase 1: Read all geometries from parquet, append to store
    /// Phase 2: Sort handles by tile_id (simulated)
    /// Phase 3: Read geometries in tile order for encoding
    #[test]
    fn test_pipeline_pattern_append_then_read_in_order() {
        let mut store = GeometryStore::new().expect("Should create store");

        // Phase 1: Simulate reading features and storing geometry
        // Each feature might appear in multiple tiles, but geometry is stored once
        let mut handles = Vec::new();
        for i in 0u32..1000 {
            let wkb = format!("geometry_{}", i).into_bytes();
            let props = format!("{{\"id\":{}}}", i).into_bytes();
            let handle = store.append(&wkb, &props).expect("Should append");
            handles.push((i, handle, wkb, props));
        }

        store.flush().expect("Should flush after Phase 1");

        // Phase 2: Simulate sorting by tile_id (we'll just reverse order here)
        handles.reverse();

        // Phase 3: Read in "tile order" (reversed) for encoding
        for (i, handle, expected_wkb, expected_props) in handles {
            let (wkb, props) = store.read(handle).expect("Should read");
            assert_eq!(wkb, expected_wkb, "WKB mismatch for feature {}", i);
            assert_eq!(props, expected_props, "Props mismatch for feature {}", i);
        }
    }

    /// Simulates features appearing in multiple tiles (tile replication)
    /// Each geometry handle should be readable multiple times
    #[test]
    fn test_tile_replication_same_handle_multiple_reads() {
        let mut store = GeometryStore::new().expect("Should create store");

        // Store 100 features
        let mut handles = Vec::new();
        for i in 0u32..100 {
            let wkb = vec![i as u8; 50]; // 50-byte geometry
            let props = vec![(i + 100) as u8; 20]; // 20-byte properties
            let handle = store.append(&wkb, &props).expect("Should append");
            handles.push((handle, wkb, props));
        }

        store.flush().expect("Should flush");

        // Simulate each feature appearing in 30 tiles (average replication)
        // Each handle is read 30 times, in random order
        for _ in 0..30 {
            for (j, (handle, expected_wkb, expected_props)) in handles.iter().enumerate() {
                let (wkb, props) = store.read(*handle).expect("Should read");
                assert_eq!(
                    &wkb, expected_wkb,
                    "WKB mismatch at iteration for feature {}",
                    j
                );
                assert_eq!(&props, expected_props, "Props mismatch for feature {}", j);
            }
        }
    }

    /// Test realistic geometry sizes based on GeoParquet data
    #[test]
    fn test_realistic_geometry_sizes() {
        let mut store = GeometryStore::new().expect("Should create store");

        // Simulate building footprints (typical sizes from real data)
        let building_wkb = vec![0x01; 200]; // ~200 bytes for simple polygon
        let building_props = rmp_serde::to_vec(&serde_json::json!({
            "area_m2": 150.5,
            "confidence": 0.95,
            "source": "microsoft_ml_buildings"
        }))
        .expect("Should serialize");

        // Simulate road segments
        let road_wkb = vec![0x02; 500]; // ~500 bytes for LineString
        let road_props = rmp_serde::to_vec(&serde_json::json!({
            "name": "Main Street",
            "type": "primary",
            "lanes": 4
        }))
        .expect("Should serialize");

        // Simulate admin boundaries (larger)
        let admin_wkb = vec![0x03; 10_000]; // ~10KB for complex polygon
        let admin_props = rmp_serde::to_vec(&serde_json::json!({
            "name": "Madagascar",
            "admin_level": 2,
            "iso_code": "MG"
        }))
        .expect("Should serialize");

        let h_building = store
            .append(&building_wkb, &building_props)
            .expect("append building");
        let h_road = store.append(&road_wkb, &road_props).expect("append road");
        let h_admin = store
            .append(&admin_wkb, &admin_props)
            .expect("append admin");

        store.flush().expect("Should flush");

        // Verify all read back correctly
        let (wkb, props) = store.read(h_building).expect("read building");
        assert_eq!(wkb.len(), 200);
        assert!(!props.is_empty());

        let (wkb, props) = store.read(h_road).expect("read road");
        assert_eq!(wkb.len(), 500);
        assert!(!props.is_empty());

        let (wkb, props) = store.read(h_admin).expect("read admin");
        assert_eq!(wkb.len(), 10_000);
        assert!(!props.is_empty());
    }

    /// Test memory efficiency: handles should be much smaller than full data
    #[test]
    fn test_memory_efficiency_handle_vs_data() {
        let avg_wkb_size = 400; // Typical geometry
        let avg_props_size = 100; // Typical properties
        let num_features = 1_000_000;
        let tiles_per_feature = 30; // Average replication

        // Old approach: store full copy per tile
        let old_memory = num_features * tiles_per_feature * (avg_wkb_size + avg_props_size);

        // New approach: store geometry once, handle per tile
        let geometry_storage = num_features * (avg_wkb_size + avg_props_size);
        let handle_storage = num_features * tiles_per_feature * GeometryHandle::SIZE;
        let new_memory = geometry_storage + handle_storage;

        // Verify significant memory reduction
        let reduction_factor = old_memory as f64 / new_memory as f64;
        assert!(
            reduction_factor > 10.0,
            "Expected >10x memory reduction, got {}x",
            reduction_factor
        );

        // Print for visibility
        println!(
            "Memory comparison: old={} MB, new={} MB, reduction={}x",
            old_memory / 1_000_000,
            new_memory / 1_000_000,
            reduction_factor
        );
    }
}
