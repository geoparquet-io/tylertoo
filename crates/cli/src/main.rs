//! CLI for gpq-tiles - Convert GeoParquet to PMTiles
//!
//! This is a thin wrapper around the gpq-tiles-core library.

use anyhow::{Context, Result};
use clap::Parser;
use gpq_tiles_core::compression::Compression;
use gpq_tiles_core::pipeline::{
    generate_tiles_to_writer_with_progress, ProgressEvent, TilerConfig,
};
use gpq_tiles_core::pmtiles_writer::StreamingPmtilesWriter;
use gpq_tiles_core::validate_wgs84;
use gpq_tiles_core::{AccumulatorConfig, AccumulatorOp, PropertyFilter};
use indicatif::{HumanBytes, HumanDuration, ProgressBar, ProgressStyle};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

mod profiling;

#[derive(Parser, Debug)]
#[command(
    name = "gpq-tiles",
    about = "Convert GeoParquet to PMTiles vector tiles",
    version
)]
struct Args {
    /// Input GeoParquet file
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

    // Add accumulator config if specified
    if let Some(acc_config) = accumulator_config {
        tiler_config = tiler_config.with_accumulator(acc_config);
    }

    // Add cluster config if specified
    if let Some((distance, maxzoom)) = cluster_config {
        tiler_config = tiler_config.with_cluster(distance, maxzoom);
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
        eprintln!();
    }

    let total_start = Instant::now();

    // Create streaming writer
    let mut writer =
        StreamingPmtilesWriter::new(compression).context("Failed to create PMTiles writer")?;

    // Run the pipeline with progress bars
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
    input: &Path,
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

    generate_tiles_to_writer_with_progress(input, config, writer, progress_callback)
        .context("Failed to generate tiles")
}
