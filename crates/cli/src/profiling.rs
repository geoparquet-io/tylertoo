//! Profiling infrastructure for gpq-tiles CLI.
//!
//! Provides two types of profiling output:
//! - Console timing summary: Shows phase-level timing breakdown
//! - Chrome trace JSON: Detailed span timing for chrome://tracing or Perfetto

use std::collections::HashMap;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Instant;
use tracing::span::{Attributes, Id};
use tracing::Subscriber;
use tracing_subscriber::layer::{Context, Layer};
use tracing_subscriber::registry::LookupSpan;

/// Timing data for a single span
#[derive(Debug, Clone)]
struct SpanTiming {
    name: String,
    start: Instant,
    duration: Option<std::time::Duration>,
}

/// Aggregated timing data for the profiling summary
#[derive(Debug, Default, Clone)]
pub struct ProfilingSummary {
    /// Total pipeline duration
    pub pipeline_duration: std::time::Duration,
    /// Per-phase timing data (name -> total duration)
    pub phase_timings: HashMap<String, std::time::Duration>,
}

impl ProfilingSummary {
    /// Print the profiling summary to stderr
    pub fn print(&self) {
        if self.pipeline_duration.is_zero() && self.phase_timings.is_empty() {
            return;
        }

        eprintln!();
        eprintln!("Profiling summary:");
        eprintln!(
            "  {:<18} {:>8.2}s  100%",
            "pipeline",
            self.pipeline_duration.as_secs_f64()
        );

        // Sort phases by duration (descending)
        let mut phases: Vec<_> = self.phase_timings.iter().collect();
        phases.sort_by(|a, b| b.1.cmp(a.1));

        for (i, (name, duration)) in phases.iter().enumerate() {
            let pct = if !self.pipeline_duration.is_zero() {
                100.0 * duration.as_secs_f64() / self.pipeline_duration.as_secs_f64()
            } else {
                0.0
            };

            let prefix = if i == phases.len() - 1 {
                "\u{2514}\u{2500}"
            } else {
                "\u{251C}\u{2500}"
            };

            eprintln!(
                "  {} {:<15} {:>8.2}s  {:>4.0}%",
                prefix,
                name,
                duration.as_secs_f64(),
                pct
            );
        }
    }
}

/// A tracing layer that collects timing data for the profiling summary
pub struct ProfilingLayer {
    /// Active span timings
    timings: Arc<Mutex<HashMap<u64, SpanTiming>>>,
    /// Completed span timings for summary
    summary: Arc<Mutex<ProfilingSummary>>,
}

impl ProfilingLayer {
    /// Create a new profiling layer
    pub fn new() -> Self {
        Self {
            timings: Arc::new(Mutex::new(HashMap::new())),
            summary: Arc::new(Mutex::new(ProfilingSummary::default())),
        }
    }

    /// Get a handle to the summary for later retrieval
    pub fn summary_handle(&self) -> Arc<Mutex<ProfilingSummary>> {
        Arc::clone(&self.summary)
    }
}

impl<S> Layer<S> for ProfilingLayer
where
    S: Subscriber + for<'a> LookupSpan<'a>,
{
    fn on_new_span(&self, attrs: &Attributes<'_>, id: &Id, _ctx: Context<'_, S>) {
        let timing = SpanTiming {
            name: attrs.metadata().name().to_string(),
            start: Instant::now(),
            duration: None,
        };
        self.timings.lock().unwrap().insert(id.into_u64(), timing);
    }

    fn on_close(&self, id: Id, _ctx: Context<'_, S>) {
        let mut timings = self.timings.lock().unwrap();
        if let Some(mut timing) = timings.remove(&id.into_u64()) {
            let duration = timing.start.elapsed();
            timing.duration = Some(duration);

            // Update summary
            let mut summary = self.summary.lock().unwrap();
            if timing.name == "pipeline" {
                summary.pipeline_duration = duration;
            } else {
                // Aggregate timing for this phase
                *summary
                    .phase_timings
                    .entry(timing.name.clone())
                    .or_default() += duration;
            }
        }
    }
}

/// Guard that prints the profiling summary when dropped
pub struct ProfilingGuard {
    summary_handle: Arc<Mutex<ProfilingSummary>>,
}

impl ProfilingGuard {
    pub fn new(summary_handle: Arc<Mutex<ProfilingSummary>>) -> Self {
        Self { summary_handle }
    }
}

impl Drop for ProfilingGuard {
    fn drop(&mut self) {
        let summary = self.summary_handle.lock().unwrap();
        summary.print();
    }
}

/// Initialize profiling with console summary output
///
/// Returns a guard that will print the summary when dropped.
pub fn init_profiling() -> ProfilingGuard {
    use tracing_subscriber::prelude::*;

    let layer = ProfilingLayer::new();
    let guard = ProfilingGuard::new(layer.summary_handle());

    let subscriber = tracing_subscriber::registry().with(layer);

    tracing::subscriber::set_global_default(subscriber).expect("Failed to set tracing subscriber");

    guard
}

/// Initialize profiling with Chrome trace JSON output
///
/// Returns a guard that will flush the trace when dropped.
pub fn init_chrome_tracing(output_path: &Path) -> impl Drop {
    use tracing_chrome::ChromeLayerBuilder;
    use tracing_subscriber::prelude::*;

    let (chrome_layer, guard) = ChromeLayerBuilder::new()
        .file(output_path)
        .include_args(true)
        .build();

    let subscriber = tracing_subscriber::registry().with(chrome_layer);

    tracing::subscriber::set_global_default(subscriber).expect("Failed to set tracing subscriber");

    guard
}

/// Initialize profiling with both console summary and Chrome trace output
pub fn init_combined_profiling(output_path: &Path) -> (ProfilingGuard, impl Drop) {
    use tracing_chrome::ChromeLayerBuilder;
    use tracing_subscriber::prelude::*;

    let profiling_layer = ProfilingLayer::new();
    let profiling_guard = ProfilingGuard::new(profiling_layer.summary_handle());

    let (chrome_layer, chrome_guard) = ChromeLayerBuilder::new()
        .file(output_path)
        .include_args(true)
        .build();

    let subscriber = tracing_subscriber::registry()
        .with(profiling_layer)
        .with(chrome_layer);

    tracing::subscriber::set_global_default(subscriber).expect("Failed to set tracing subscriber");

    (profiling_guard, chrome_guard)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_profiling_summary_print() {
        let mut phase_timings = HashMap::new();
        phase_timings.insert(
            "read_parquet".to_string(),
            std::time::Duration::from_secs(3),
        );
        phase_timings.insert("sort".to_string(), std::time::Duration::from_secs(2));
        phase_timings.insert("encode".to_string(), std::time::Duration::from_secs(4));

        let summary = ProfilingSummary {
            pipeline_duration: std::time::Duration::from_secs(10),
            phase_timings,
        };

        // This just tests that print doesn't panic
        summary.print();
    }
}
