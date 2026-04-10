//! CLI for gpq-tiles - Convert GeoParquet to PMTiles
//!
//! This is a thin wrapper around the gpq-tiles-core library.

use anyhow::{Context, Result};
use clap::Parser;
use gpq_tiles_core::batch_processor::total_parquet_size;
use gpq_tiles_core::compression::Compression;
use gpq_tiles_core::parse_bounds;
use gpq_tiles_core::pipeline::{
    auto_processing_mode, generate_tiles_to_writer_with_progress, ProcessingMode, ProgressEvent,
    TilerConfig,
};
use gpq_tiles_core::pmtiles_writer::StreamingPmtilesWriter;
use gpq_tiles_core::validate_wgs84;
use gpq_tiles_core::{AccumulatorConfig, AccumulatorOp, PropertyFilter};
use indicatif::{HumanBytes, HumanDuration, ProgressBar, ProgressStyle};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

mod profiling;

/// Parse human-readable memory size (e.g., "8G", "16G", "512M") to bytes.
fn parse_memory_size(s: &str) -> Result<usize, String> {
    let s = s.trim().to_uppercase();
    let (num_str, multiplier) = if s.ends_with("G") || s.ends_with("GB") {
        (
            s.trim_end_matches("GB").trim_end_matches("G"),
            1024 * 1024 * 1024,
        )
    } else if s.ends_with("M") || s.ends_with("MB") {
        (s.trim_end_matches("MB").trim_end_matches("M"), 1024 * 1024)
    } else if s.ends_with("K") || s.ends_with("KB") {
        (s.trim_end_matches("KB").trim_end_matches("K"), 1024)
    } else {
        // Assume bytes if no suffix
        (s.as_str(), 1)
    };

    num_str
        .trim()
        .parse::<usize>()
        .map(|n| n * multiplier)
        .map_err(|_| {
            format!(
                "Invalid memory size: '{}'. Use format like '8G', '16G', '512M'",
                s
            )
        })
}

/// Parse human-readable size (e.g., "500K", "1M", "2G") to bytes as u32.
fn parse_size_bytes(s: &str) -> Result<u32, String> {
    let bytes = parse_memory_size(s)?;
    u32::try_from(bytes).map_err(|_| format!("Size {} too large for u32", s))
}

#[derive(Parser, Debug)]
#[command(
    name = "gpq-tiles",
    about = "Convert GeoParquet to PMTiles vector tiles",
    version
)]
struct Args {
    /// Input GeoParquet file or directory
    ///
    /// If a directory, recursively finds all .parquet files and processes them
    /// as a single logical dataset (no schema validation).
    #[arg(value_name = "INPUT")]
    input: PathBuf,

    /// Output PMTiles file
    #[arg(value_name = "OUTPUT")]
    output: PathBuf,

    /// Minimum zoom level
    #[arg(long, default_value = "0")]
    min_zoom: u8,

    /// Maximum zoom level
    #[arg(long, default_value = "14")]
    max_zoom: u8,

    /// Feature dropping density (low, medium, high)
    #[arg(long, default_value = "medium")]
    drop_density: String,

    /// Enable gap-based density dropping (tippecanoe-compatible).
    ///
    /// Uses Hilbert index gaps to determine which features to drop,
    /// providing better preservation of spatial distribution than
    /// grid-based dropping. Equivalent to tippecanoe's --drop-densest-as-needed.
    ///
    /// When enabled without --gamma, uses gamma=2.0 (tippecanoe default).
    #[arg(long)]
    drop_densest_as_needed: bool,

    /// Gamma parameter for gap-based density dropping.
    ///
    /// Controls the exponential spacing for feature selection:
    /// - gamma=0: Disabled (use grid-based instead)
    /// - gamma=1: Linear spacing
    /// - gamma=2: "Reduces dots < 1 pixel apart to square root of original"
    ///   (tippecanoe default when using --drop-densest-as-needed)
    /// - Higher values = more aggressive dropping of closely-spaced features
    ///
    /// Implies --drop-densest-as-needed when set.
    #[arg(long)]
    gamma: Option<f64>,

    /// Enable size-based feature dropping (tippecanoe parity).
    ///
    /// When a tile has more features than can be rendered clearly,
    /// drops the smallest features (by pixel area) first.
    /// Equivalent to tippecanoe's --drop-smallest-as-needed.
    #[arg(long)]
    drop_smallest_as_needed: bool,

    /// Minimum pixel area for --drop-smallest-as-needed (default: 4.0).
    ///
    /// Features with pixel area below this threshold are candidates for dropping.
    #[arg(long, default_value = "4.0")]
    drop_smallest_threshold: f64,

    /// Maximum tile size in bytes (e.g., "500K", "1M").
    ///
    /// When a tile exceeds this limit, adaptive thresholds increase
    /// to drop more features until the tile fits. Equivalent to
    /// tippecanoe's --maximum-tile-bytes.
    #[arg(long, value_parser = parse_size_bytes)]
    max_tile_size: Option<u32>,

    /// Maximum features per tile.
    ///
    /// When a tile exceeds this limit, adaptive thresholds increase
    /// to drop more features until the tile fits. Equivalent to
    /// tippecanoe's --maximum-tile-features.
    #[arg(long)]
    max_tile_features: Option<u32>,

    /// Layer name for the output tiles (default: derived from input filename)
    #[arg(long)]
    layer_name: Option<String>,

    /// Include only specified properties in output tiles (whitelist).
    /// Can be specified multiple times. Matches tippecanoe's -y flag.
    /// Example: --include name --include population
    #[arg(short = 'y', long = "include", value_name = "FIELD")]
    include: Vec<String>,

    /// Exclude specified properties from output tiles (blacklist).
    /// Can be specified multiple times. Matches tippecanoe's -x flag.
    /// Example: --exclude internal_id --exclude temp_field
    #[arg(short = 'x', long = "exclude", value_name = "FIELD")]
    exclude: Vec<String>,

    /// Exclude all properties, keeping only geometry.
    /// Matches tippecanoe's -X flag.
    #[arg(short = 'X', long = "exclude-all")]
    exclude_all: bool,

    /// Define attribute accumulation for feature merging.
    /// Format: ATTRIBUTE:OPERATION (e.g., population:sum, names:comma).
    /// Matches tippecanoe's -ac flag.
    ///
    /// Supported operations: sum, product, mean, max, min, concat, comma, count
    ///
    /// Example: --accumulate population:sum --accumulate names:comma
    #[arg(long = "accumulate", value_name = "ATTR:OP")]
    accumulate: Vec<String>,

    /// Cluster points within a specified distance (in 256-pixel tile units).
    /// Matches tippecanoe's --cluster-distance flag.
    ///
    /// When set, nearby points are grouped together and replaced with a single
    /// point at their centroid. Properties are accumulated according to
    /// --accumulate settings.
    ///
    /// Typical values: 50 (default in tippecanoe), 25 (less aggressive), 100 (more aggressive).
    /// Requires --cluster-maxzoom to also be set.
    #[arg(long = "cluster-distance", value_name = "PIXELS")]
    cluster_distance: Option<u32>,

    /// Maximum zoom level at which to cluster points.
    /// Matches tippecanoe's --cluster-maxzoom flag.
    ///
    /// At zoom levels above this, points are kept separate.
    /// Typically set to max_zoom - 2 or so.
    /// Requires --cluster-distance to also be set.
    #[arg(long = "cluster-maxzoom", value_name = "ZOOM")]
    cluster_maxzoom: Option<u8>,

    /// Enable geometry coalescing for dense tiles.
    ///
    /// Merges features into Multi* geometries to reduce tile complexity while
    /// preserving all coordinate data. Uses GeoParquet metadata to predict dense
    /// tiles upfront (no retry loops like tippecanoe).
    ///
    /// Only the densest row groups (top 10% by default) are coalesced.
    #[arg(long = "coalesce-densest-as-needed")]
    coalesce_densest: bool,

    /// Set the percentile threshold for coalescing (default: 90).
    ///
    /// Only row groups in the top (100 - percentile)% densest are coalesced.
    /// Example: 90 means top 10% densest, 75 means top 25% densest.
    /// Requires --coalesce-densest-as-needed.
    #[arg(
        long = "coalesce-percentile",
        value_name = "PERCENTILE",
        default_value = "90"
    )]
    coalesce_percentile: u8,

    /// Minimum features per tile to trigger coalescing (default: 100).
    ///
    /// Even if a tile exceeds the percentile threshold, coalescing is
    /// skipped if the feature count is below this value.
    /// Requires --coalesce-densest-as-needed.
    #[arg(
        long = "coalesce-min-density",
        value_name = "FEATURES",
        default_value = "100"
    )]
    coalesce_min_density: f64,

    /// Attribute handling mode during coalescing (default: drop).
    ///
    /// Controls how feature attributes are handled when geometries are merged:
    /// - drop: Discard all attributes (tippecanoe-compatible default)
    /// - keep-first: Keep the first feature's attributes
    ///
    /// Requires --coalesce-densest-as-needed.
    #[arg(long = "coalesce-attrs", value_name = "MODE", default_value = "drop")]
    coalesce_attrs: String,

    /// Enable zoom-dependent geometry simplification.
    ///
    /// Applies Douglas-Peucker simplification with tolerance scaling by zoom level.
    /// Dramatically reduces tile sizes for linear features (roads, rivers, boundaries).
    /// At lower zoom levels (zoomed out), more aggressive simplification is applied.
    #[arg(long)]
    simplify: bool,

    /// Simplification factor (default: 1.0 = 1 pixel tolerance).
    ///
    /// Controls aggressiveness: 0.5 = more detail, 1.0 = standard, 2.0 = aggressive.
    /// Only used when --simplify is enabled.
    #[arg(long, default_value = "1.0")]
    simplify_factor: f64,

    /// Enable automatic per-feature max zoom based on feature area.
    ///
    /// Large features (e.g., country polygons) stop at low zoom levels where they
    /// would otherwise create millions of tiles. Calculates both min zoom (when features
    /// become visible) and max zoom (when they would explode). This prevents both tile
    /// explosion and visual clutter.
    ///
    /// Example: A 100m² building appears at z8, goes to z14. A 1000km² country appears
    /// at z0, stops at z7.
    #[arg(long)]
    zoom_by_area: bool,

    /// Maximum tiles threshold for --zoom-by-area (default: 400).
    ///
    /// Features STOP when they would cover more than this many tiles.
    /// 400 ≈ 20x20 grid. Higher values = features continue longer (more tiles).
    /// Typical: 100 (conservative), 400 (balanced), 1000 (aggressive).
    #[arg(long, default_value = "400")]
    max_tile_threshold: u32,

    /// Minimum pixel area for --zoom-by-area (default: 4.0 sq pixels).
    ///
    /// Features START when they're >= this many square pixels (visible).
    /// 4.0 = 2x2 pixel square. Higher values = features appear later (less clutter).
    #[arg(long, default_value = "4.0")]
    min_pixel_area: f64,

    /// Compression algorithm for tiles (gzip, zstd, brotli, none)
    ///
    /// Gzip is the default for maximum compatibility with PMTiles viewers.
    /// Use --compression zstd for faster encoding when your viewer supports it.
    #[arg(long, default_value = "gzip")]
    compression: String,

    /// Enable verbose logging with progress bars
    #[arg(short, long)]
    verbose: bool,

    /// Enable deterministic (sequential) processing for reproducible output.
    ///
    /// When enabled, disables parallel processing to ensure bit-exact
    /// reproducibility across runs. Useful for debugging, testing, and
    /// compliance workflows. Significantly slower than parallel processing.
    #[arg(long)]
    deterministic: bool,

    /// Enable profiling output with timing summary
    ///
    /// Shows phase-level timing breakdown after conversion completes:
    /// - read_parquet: Time spent reading GeoParquet and clipping
    /// - sort: Time spent in external merge sort
    /// - encode: Time spent encoding tiles to MVT format
    #[arg(long)]
    profile: bool,

    /// Write Chrome trace JSON to file for visualization
    ///
    /// The output can be viewed in Chrome's chrome://tracing or Perfetto.
    /// This captures detailed span timing for all pipeline phases.
    #[arg(long, value_name = "FILE")]
    trace_output: Option<PathBuf>,

    /// Spatial filter to skip row groups outside bounding box.
    ///
    /// Accepts either tile coordinates (z/x/y) or WGS84 bbox (xmin,ymin,xmax,ymax).
    /// Row groups whose bounding boxes don't intersect this filter are skipped
    /// entirely, which can dramatically reduce processing time for bounded extracts.
    ///
    /// Examples:
    ///   --bounds 10/163/395           (tile coordinates for SF area)
    ///   --bounds -122.5,37.7,-122.3,37.9  (WGS84 bbox)
    #[arg(long, value_name = "BOUNDS")]
    bounds: Option<String>,

    /// Number of spatial buckets for memory-bounded processing.
    ///
    /// When processing large files (>10GB), the pipeline partitions records
    /// into spatial buckets to bound memory usage. By default, bucket count
    /// is auto-tuned based on file size. Use this flag to override.
    ///
    /// Typical values: 64 (small), 256 (medium), 1024 (large files).
    #[arg(long, value_name = "N")]
    buckets: Option<usize>,

    /// Memory budget for sorting (e.g., "8G", "16G", "40G").
    ///
    /// Controls how much RAM the external sort can use per bucket.
    /// Larger values = fewer temp files = faster sorting and avoids
    /// "too many open files" errors on large datasets.
    ///
    /// Default: conservative (creates many small temp files).
    /// Recommended: 50-75% of available RAM for large datasets.
    #[arg(long, value_name = "SIZE", value_parser = parse_memory_size)]
    memory_budget: Option<usize>,
}

impl Args {
    fn parse_property_filter(&self) -> Result<PropertyFilter> {
        // Check for conflicting options
        let has_include = !self.include.is_empty();
        let has_exclude = !self.exclude.is_empty();

        if self.exclude_all && (has_include || has_exclude) {
            anyhow::bail!("Cannot use --exclude-all with --include or --exclude");
        }

        if has_include && has_exclude {
            anyhow::bail!("Cannot use --include and --exclude together");
        }

        if self.exclude_all {
            Ok(PropertyFilter::ExcludeAll)
        } else if has_include {
            Ok(PropertyFilter::include(self.include.clone()))
        } else if has_exclude {
            Ok(PropertyFilter::exclude(self.exclude.clone()))
        } else {
            Ok(PropertyFilter::None)
        }
    }

    fn parse_compression(&self) -> Result<Compression> {
        Compression::from_str(&self.compression).ok_or_else(|| {
            anyhow::anyhow!(
                "Invalid compression: '{}'. Valid options: none, gzip, brotli, zstd",
                self.compression
            )
        })
    }

    /// Parse --accumulate arguments into AccumulatorConfig.
    ///
    /// Format: ATTRIBUTE:OPERATION (e.g., population:sum)
    fn parse_accumulator_config(&self) -> Result<Option<AccumulatorConfig>> {
        if self.accumulate.is_empty() {
            return Ok(None);
        }

        let mut config = AccumulatorConfig::new();

        for spec in &self.accumulate {
            let parts: Vec<&str> = spec.splitn(2, ':').collect();
            if parts.len() != 2 {
                anyhow::bail!(
                    "Invalid accumulate format: '{}'. Expected ATTRIBUTE:OPERATION (e.g., population:sum)",
                    spec
                );
            }

            let attribute = parts[0];
            let op_str = parts[1];

            let op = AccumulatorOp::parse(op_str).ok_or_else(|| {
                anyhow::anyhow!(
                    "Invalid accumulator operation: '{}'. Valid operations: sum, product, mean, max, min, concat, comma, count",
                    op_str
                )
            })?;

            config.set_operation(attribute, op);
        }

        Ok(Some(config))
    }

    /// Parse --cluster-distance and --cluster-maxzoom into cluster configuration.
    ///
    /// Both flags must be specified together or neither.
    fn parse_cluster_config(&self) -> Result<Option<(u32, u8)>> {
        match (self.cluster_distance, self.cluster_maxzoom) {
            (Some(distance), Some(maxzoom)) => Ok(Some((distance, maxzoom))),
            (None, None) => Ok(None),
            (Some(_), None) => {
                anyhow::bail!("--cluster-distance requires --cluster-maxzoom to also be set")
            }
            (None, Some(_)) => {
                anyhow::bail!("--cluster-maxzoom requires --cluster-distance to also be set")
            }
        }
    }
}

fn main() -> Result<()> {
    // Initialize dhat profiler if feature is enabled
    // This must be at the very start of main() - the profiler outputs
    // dhat-heap.json on Drop (program exit)
    #[cfg(feature = "dhat-heap")]
    let _profiler = dhat::Profiler::new_heap();

    let args = Args::parse();

    // Initialize profiling if requested (must happen before any tracing calls)
    // Store guards to keep them alive for the duration of main()
    let _profiling_guard: Option<profiling::ProfilingGuard>;
    let _chrome_guard: Option<Box<dyn std::any::Any>>;

    match (&args.profile, &args.trace_output) {
        (true, Some(trace_path)) => {
            // Both console profiling and Chrome trace
            let (pg, cg) = profiling::init_combined_profiling(trace_path);
            _profiling_guard = Some(pg);
            _chrome_guard = Some(Box::new(cg));
        }
        (true, None) => {
            // Console profiling only
            _profiling_guard = Some(profiling::init_profiling());
            _chrome_guard = None;
        }
        (false, Some(trace_path)) => {
            // Chrome trace only
            _profiling_guard = None;
            _chrome_guard = Some(Box::new(profiling::init_chrome_tracing(trace_path)));
        }
        (false, None) => {
            // No profiling
            _profiling_guard = None;
            _chrome_guard = None;
        }
    }

    // Initialize logging - suppress when verbose (we use progress bars instead)
    // Also suppress when profiling (tracing handles output instead)
    let log_level = if args.verbose || args.profile {
        "warn"
    } else {
        "info"
    };
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or(log_level)).init();

    // Parse options
    let property_filter = args
        .parse_property_filter()
        .context("Failed to parse property filter")?;
    let compression = args
        .parse_compression()
        .context("Failed to parse compression")?;
    let accumulator_config = args
        .parse_accumulator_config()
        .context("Failed to parse accumulator config")?;
    let cluster_config = args
        .parse_cluster_config()
        .context("Failed to parse cluster config")?;

    // Validate input file uses WGS84 (EPSG:4326) coordinates
    validate_wgs84(&args.input).map_err(|e| anyhow::anyhow!("{}", e))?;

    // Derive layer name from input filename if not specified
    let layer_name = args.layer_name.clone().unwrap_or_else(|| {
        args.input
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("layer")
            .to_string()
    });

    // Build TilerConfig - always quiet since we use progress bars
    let mut tiler_config = TilerConfig::new(args.min_zoom, args.max_zoom)
        .with_extent(4096)
        .with_layer_name(&layer_name)
        .with_property_filter(property_filter)
        .with_quiet(true) // Suppress log output when we have progress bars
        .with_deterministic(args.deterministic);

    // Configure gap-based density dropping if requested
    // --gamma takes precedence, otherwise --drop-densest-as-needed uses gamma=2.0
    if let Some(gamma) = args.gamma {
        tiler_config = tiler_config.with_gamma(gamma);
    } else if args.drop_densest_as_needed {
        tiler_config = tiler_config.with_drop_densest_as_needed();
    }

    // Configure size-based feature dropping if requested
    if args.drop_smallest_as_needed {
        tiler_config = tiler_config
            .with_drop_smallest_as_needed()
            .with_drop_smallest_threshold(args.drop_smallest_threshold);
    }

    // Configure adaptive threshold limits if specified
    if let Some(max_size) = args.max_tile_size {
        tiler_config = tiler_config.with_max_tile_size(max_size);
    }
    if let Some(max_features) = args.max_tile_features {
        tiler_config = tiler_config.with_max_tile_features(max_features);
    }

    // Add accumulator config if specified
    if let Some(acc_config) = accumulator_config {
        tiler_config = tiler_config.with_accumulator(acc_config);
    }

    // Add cluster config if specified
    if let Some((distance, maxzoom)) = cluster_config {
        tiler_config = tiler_config.with_cluster(distance, maxzoom);
    }

    // Add coalesce config if specified
    if args.coalesce_densest {
        tiler_config = tiler_config
            .with_coalesce_percentile(args.coalesce_percentile)
            .with_coalesce_min_density(args.coalesce_min_density);

        // Parse attribute handling mode
        let attr_mode = match args.coalesce_attrs.to_lowercase().as_str() {
            "drop" => gpq_tiles_core::coalesce::AttributeMode::Drop,
            "keep-first" | "keepfirst" => gpq_tiles_core::coalesce::AttributeMode::KeepFirst,
            "strict" => gpq_tiles_core::coalesce::AttributeMode::Strict,
            other => {
                anyhow::bail!(
                    "Invalid --coalesce-attrs value: '{}'. Valid options: drop, keep-first, strict",
                    other
                );
            }
        };
        tiler_config = tiler_config.with_coalesce_attribute_mode(attr_mode);
    }

    // Configure simplification if requested
    if args.simplify {
        tiler_config = tiler_config.with_simplify(args.simplify_factor);
    }

    // Configure zoom-by-area if requested
    if args.zoom_by_area {
        tiler_config.zoom_by_area = true;
        tiler_config.max_tile_threshold = args.max_tile_threshold;
        tiler_config.min_pixel_area = args.min_pixel_area;
    }

    // Configure spatial filter (--bounds) if specified
    if let Some(ref bounds_str) = args.bounds {
        let spatial_filter = parse_bounds(bounds_str)
            .map_err(|e| anyhow::anyhow!("Invalid bounds '{}': {}", bounds_str, e))?;
        tiler_config = tiler_config.with_spatial_filter(spatial_filter);
    }

    // Configure processing mode (auto-tuned or explicit buckets)
    // Get total parquet size (handles both files and directories)
    let file_size = total_parquet_size(&args.input);

    if let Some(num_buckets) = args.buckets {
        // Explicit bucket count
        tiler_config = tiler_config.with_processing_mode(ProcessingMode::bucketed(num_buckets));
    } else {
        // Auto-tune based on file size (bucketing for files >= 10GB)
        let mode = auto_processing_mode(file_size);
        tiler_config = tiler_config.with_processing_mode(mode);
    }

    // Configure memory budget if specified
    if let Some(budget) = args.memory_budget {
        tiler_config = tiler_config.with_memory_budget(budget);
    }

    // Print configuration in verbose mode
    if args.verbose {
        eprintln!("Configuration:");
        eprintln!("  Input: {}", args.input.display());
        eprintln!("  Output: {}", args.output.display());
        eprintln!("  Zoom: {}-{}", args.min_zoom, args.max_zoom);
        eprintln!("  Compression: {}", args.compression);
        if let Some(gamma) = tiler_config.gamma {
            eprintln!("  Density dropping: gap-based (gamma={})", gamma);
        }
        if args.deterministic {
            eprintln!("  Processing: deterministic (sequential)");
        }
        if let Some(ref acc_config) = tiler_config.accumulator_config {
            eprintln!("  Accumulators:");
            for (attr, op) in acc_config.operations() {
                eprintln!("    {}: {}", attr, op);
            }
        }
        if let Some(ref cluster) = tiler_config.cluster_config {
            eprintln!(
                "  Clustering: distance={}, max_zoom={}",
                cluster.distance, cluster.max_zoom
            );
        }
        if let Some(factor) = tiler_config.simplify_factor {
            eprintln!("  Simplification: enabled (factor={})", factor);
        }
        if tiler_config.zoom_by_area {
            eprintln!(
                "  Zoom by area: enabled (max_tiles={}, min_pixels={:.1})",
                tiler_config.max_tile_threshold, tiler_config.min_pixel_area
            );
        }
        if let Some(ref filter) = tiler_config.spatial_filter {
            eprintln!(
                "  Spatial filter: [{:.4}, {:.4}, {:.4}, {:.4}]",
                filter.lng_min, filter.lat_min, filter.lng_max, filter.lat_max
            );
        }
        match &tiler_config.processing_mode {
            ProcessingMode::InMemory => {
                eprintln!("  Processing mode: in-memory");
            }
            ProcessingMode::Bucketed { num_buckets } => {
                if let Some(n) = num_buckets {
                    eprintln!("  Processing mode: bucketed ({} buckets)", n);
                } else {
                    eprintln!("  Processing mode: bucketed (auto-tuned)");
                }
            }
        }
        if let Some(budget) = tiler_config.memory_budget {
            eprintln!("  Memory budget: {}", HumanBytes(budget as u64));
        }
        eprintln!();
    }

    let total_start = Instant::now();

    // Create streaming writer
    let mut writer =
        StreamingPmtilesWriter::new(compression).context("Failed to create PMTiles writer")?;

    // Run the pipeline with progress bars (supports both files and directories)
    let stats = run_with_progress(&args.input, &tiler_config, &mut writer, args.verbose)?;

    // Finalize PMTiles file
    let write_stats = writer
        .finalize(&args.output)
        .context("Failed to write PMTiles file")?;

    let total_duration = total_start.elapsed();

    // Print succinct summary
    print_summary(
        &args.input,
        &args.output,
        &write_stats,
        stats.peak_bytes,
        total_duration,
        args.verbose,
    );

    Ok(())
}

/// Print a succinct summary of the conversion
fn print_summary(
    input: &Path,
    output: &Path,
    write_stats: &gpq_tiles_core::pmtiles_writer::StreamingWriteStats,
    peak_memory: usize,
    duration: Duration,
    verbose: bool,
) {
    let tiles_per_sec = write_stats.total_tiles as f64 / duration.as_secs_f64();

    println!();
    println!(
        "✓ Converted {} → {}",
        input.file_name().unwrap_or_default().to_string_lossy(),
        output.file_name().unwrap_or_default().to_string_lossy()
    );
    println!(
        "  {:>12} tiles in {} ({:.0} tiles/sec)",
        format_number(write_stats.total_tiles),
        HumanDuration(duration),
        tiles_per_sec
    );
    println!("  {:>12} peak memory", HumanBytes(peak_memory as u64));

    if verbose {
        println!();
        println!("Details:");
        println!(
            "  Unique tiles: {} ({:.1}% dedup)",
            format_number(write_stats.unique_tiles),
            100.0 * (1.0 - write_stats.unique_tiles as f64 / write_stats.total_tiles.max(1) as f64)
        );
        println!("  Output size:  {}", HumanBytes(write_stats.bytes_written));
        println!(
            "  Dedup saved:  {}",
            HumanBytes(write_stats.bytes_saved_dedup)
        );
    }
}

/// Format a number with thousands separators
fn format_number(n: u64) -> String {
    let s = n.to_string();
    let mut result = String::new();
    for (i, c) in s.chars().rev().enumerate() {
        if i > 0 && i % 3 == 0 {
            result.push(',');
        }
        result.push(c);
    }
    result.chars().rev().collect()
}

/// Run tile generation with progress bars for ExternalSort mode
fn run_with_progress(
    input_path: &Path,
    config: &TilerConfig,
    writer: &mut StreamingPmtilesWriter,
    verbose: bool,
) -> Result<gpq_tiles_core::memory::MemoryStats> {
    use indicatif::MultiProgress;

    // Multi-progress for managing multiple progress bars
    let multi = MultiProgress::new();

    // Shared state for progress bars
    let phase1_pb: Arc<Mutex<Option<ProgressBar>>> = Arc::new(Mutex::new(None));
    let phase2_pb: Arc<Mutex<Option<ProgressBar>>> = Arc::new(Mutex::new(None));
    let phase3_pb: Arc<Mutex<Option<ProgressBar>>> = Arc::new(Mutex::new(None));

    let phase1_pb_clone = Arc::clone(&phase1_pb);
    let phase2_pb_clone = Arc::clone(&phase2_pb);
    let phase3_pb_clone = Arc::clone(&phase3_pb);
    let multi_clone = multi.clone();

    // Track totals for Phase 3
    let total_records: Arc<Mutex<u64>> = Arc::new(Mutex::new(0));
    let total_records_clone = Arc::clone(&total_records);
    let total_row_groups: Arc<Mutex<usize>> = Arc::new(Mutex::new(0));
    let _total_row_groups_clone = Arc::clone(&total_row_groups);

    let progress_callback = Box::new(move |event: ProgressEvent| {
        match event {
            ProgressEvent::PhaseStart { phase, name: _ } => {
                if phase == 1 {
                    // Phase 1: Reading row groups - will become determinate when we know total
                    let pb = multi_clone.add(ProgressBar::new_spinner());
                    pb.set_style(
                        ProgressStyle::default_spinner()
                            .template("{spinner:.cyan} Reading GeoParquet...")
                            .unwrap(),
                    );
                    pb.enable_steady_tick(Duration::from_millis(100));
                    *phase1_pb_clone.lock().unwrap() = Some(pb);
                } else if phase == 3 {
                    // Phase 3: Encoding tiles - determinate
                    let total = *total_records_clone.lock().unwrap();
                    let pb = multi_clone.add(ProgressBar::new(total));
                    pb.set_style(
                        ProgressStyle::default_bar()
                            .template("{spinner:.cyan} Encoding tiles [{bar:40.cyan/blue}] {pos}/{len} ({percent}%)")
                            .unwrap()
                            .progress_chars("█▓▒░  "),
                    );
                    *phase3_pb_clone.lock().unwrap() = Some(pb);
                }
            }

            ProgressEvent::Phase1Progress {
                row_group,
                total_row_groups: total_rg,
                features_in_group: _,
                records_written,
            } => {
                // Store total row groups for progress calculation
                *total_row_groups.lock().unwrap() = total_rg;

                if let Some(ref pb) = *phase1_pb_clone.lock().unwrap() {
                    // Convert spinner to progress bar once we know the total
                    // row_group is now a completed count (1, 2, 3...) not an index
                    if row_group == 1 {
                        pb.set_length(total_rg as u64);
                        pb.set_style(
                            ProgressStyle::default_bar()
                                .template("{spinner:.cyan} Reading [{bar:40.cyan/blue}] {pos}/{len} row groups | {msg}")
                                .unwrap()
                                .progress_chars("█▓▒░  "),
                        );
                    }
                    pb.set_position(row_group as u64);
                    pb.set_message(format!("{} records", format_number(records_written)));
                }
            }

            ProgressEvent::Phase1Complete {
                total_records: total,
                peak_memory_bytes: _,
            } => {
                // Store total for Phase 3
                *total_records.lock().unwrap() = total;

                if let Some(ref pb) = *phase1_pb_clone.lock().unwrap() {
                    pb.finish_with_message(format!("✓ {} records", format_number(total)));
                }
            }

            ProgressEvent::Phase2Start => {
                let pb = multi_clone.add(ProgressBar::new_spinner());
                pb.set_style(
                    ProgressStyle::default_spinner()
                        .template("{spinner:.cyan} Sorting by tile ID...")
                        .unwrap(),
                );
                pb.enable_steady_tick(Duration::from_millis(100));
                *phase2_pb_clone.lock().unwrap() = Some(pb);
            }

            ProgressEvent::Phase2Complete => {
                if let Some(ref pb) = *phase2_pb_clone.lock().unwrap() {
                    pb.finish_with_message("✓ Sorted");
                }
            }

            ProgressEvent::Phase3Progress {
                tiles_written,
                records_processed,
                total_records: _,
            } => {
                if let Some(ref pb) = *phase3_pb_clone.lock().unwrap() {
                    pb.set_position(records_processed);
                    if tiles_written % 10000 == 0 {
                        pb.set_message(format!("{} tiles", format_number(tiles_written)));
                    }
                }
            }

            ProgressEvent::Complete {
                total_tiles: _,
                peak_memory_bytes: _,
                duration_secs: _,
            } => {
                if let Some(ref pb) = *phase3_pb_clone.lock().unwrap() {
                    pb.finish_with_message("✓ Complete");
                }
            }
        }
    });

    let _ = verbose; // Reserved for future use (sub-progress for large geometries)

    // Standard pipeline handles both files and directories transparently
    generate_tiles_to_writer_with_progress(input_path, config, writer, progress_callback)
        .context("Failed to generate tiles")
}
