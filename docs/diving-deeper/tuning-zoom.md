# Tuning what appears at each zoom

Zoom out on a country of field boundaries and the map cannot draw every polygon;
the features would collapse into noise long before they finished rendering.
Something has to give, and tylertoo decides what. This topic is the mental model
for those decisions — the quality ladder — organized by the choice each knob
makes rather than alphabetically. The generated CLI reference lists every flag
with its exact default. The goal here is understanding, so you know which knob
to reach for when the output looks wrong.

Read the groups below in order. They roughly follow the pipeline: rank the
features, join line fragments, gate out the invisible, thin to one per cell,
budget the survivors, then simplify what remains.

## Design decisions

**Each zoom level is generalized independently.** Every level computes its own
thinning grid, visibility gate, and simplification tolerance from that level's
ground sample distance. What you see at z6 is a deliberate generalization built
for z6, not a side effect of down-scaling the finest data. This is why the knobs
below all scale with GSD rather than taking absolute pixel values.

**Class ranking decides which feature wins a cell.** Thinning keeps one feature
per grid cell per level, and the winner is chosen by rank first, then size, then
a stable hash. Ranking therefore sits upstream of every thinning and density
decision. Get it right and a motorway survives where an alley is dropped; leave
it wrong and the map keeps the wrong features at every coarse zoom. It is the
highest-leverage quality knob in the set.

**Overture columns auto-detect a class ranking.** When the input carries
recognizable Overture class columns, the converter applies a sensible ranking
without configuration, so roads and places thin in a familiar order out of the
box. `--no-auto-rank` turns the detection off when you want size-only ordering
or a ranking of your own.

**The canonical finest level renders verbatim.** The maximum-zoom level holds
your features at full detail, and the ladder only generalizes the levels coarser
than it. No knob in this topic touches the finest data. Tuning changes how the
map reads on the way in, never what it resolves to up close.

**Defaults come from corpus sweeps.** The default values — drop rate 1.65,
visibility 2.0, line thinning 1.0 — are measured, not guessed. Each came from a
sweep over real data, such as Portland's road network or a dense point layer,
recorded in `corpus/SWEEPS.md`. Reach for a knob when your data differs from that
corpus, not because the defaults are placeholders.

## API walkthrough

### Setting the detail ladder

**`--gsd-base`.** The master detail knob. It maps zoom to resolution by
`gsd(z) = 40075016.69 / base / 2^z`, so a larger base makes every level's GSD
smaller, which keeps more features and more vertices at a given zoom for denser,
heavier levels. A smaller base does the reverse for sparser, cheaper coarse
levels. It scales the whole ladder at once, before any per-knob adjustment.

**`--gsd` / `--min-zoom` / `--max-zoom`.** The ladder's rungs. Zoom bounds are
the usual way to set them; an explicit `--gsd` list drives resolution directly
in meters when you need absolute control.

### Choosing which feature wins a cell

**`--class-rank <spec>`.** Ranks feature classes so that important ones win the
per-cell contest during thinning. This is where you encode that highways outrank
service roads, or that buildings outrank fences, so the coarse map keeps the
features a reader expects.

**`--sort-key <col>`.** A numeric column to order by when the data has no class
to rank, such as population or area. It and `--class-rank` are mutually
exclusive, since each defines the same tie-break differently.

**`--no-auto-rank`.** Disables Overture class auto-detection, falling back to
size ordering. Use it when the auto-detected ranking fights your data.

### Dropping features below a size

**`--polygon-visibility` / `--line-visibility`.** A hard size gate in GSD
multiples. A feature is eligible for a level only if its bounding-box diagonal is
at least the factor times that level's GSD; below it, the feature is dropped, not
shrunk. A larger factor drops more small features at coarse levels for a sparser
map; a smaller one keeps more. Both default to 2.0, retuned down from 4.0 once
write-time simplification was already removing sub-tolerance polygons.

### Thinning to one feature per cell

**`--point-thinning` / `--line-thinning` / `--polygon-thinning`.** Grid-cell
thinning, where the cell size is the factor times the level's GSD and one
feature survives per cell. A bigger factor means bigger cells and a sparser
level; a smaller factor packs more in. Points default to 4.0, lines and polygons
to 1.0, because points crowd a tile fastest and polygons least. Point thinning
jumps to 16.0 under `--cluster`, since absorbed points are summarized rather than
lost and the coarser grid gives the familiar graduated-dot look.

### Budgeting density at mid zooms

**`--drop-rate`.** A per-level feature budget that decays toward coarse zooms by
`budget(L) = N / rate^(finest − L)`. Cell-winner thinning stops binding around
mid zoom, where features spread out enough that every cell has a winner, which
leaves mid-zoom counts plateauing near the full dataset. The budget cuts the
lowest-priority survivors until each level meets it, so the plateau comes down. A
bigger rate sheds harder for sparser mid zooms and smaller files. The finest
level is never dropped.

**`--drop-gamma`.** Spatial fairness for that budget. Rather than a single global
rank cut, which would empty the countryside to keep the cities, the budget is
shared across neighborhood super-cells so dense and sparse areas both retain
their top features. It redistributes survivors without changing per-level totals,
so it tunes independently of `--drop-rate`.

**`--no-density-drop`.** The off switch. It reverts to pure cell-winner thinning
and emits an identical footer, for comparing before and against, or when
cell-winner thinning already meets your needs.

### Simplifying per-feature geometry

**`--simplify-factor`.** How much vertex detail each coarse level sheds, as a
Ramer-Douglas-Peucker tolerance of factor times GSD. A lower factor keeps more
vertices for crisper but heavier levels; a higher one crudens for lighter ones.
A feature whose bounding-box diagonal falls below the tolerance is dropped
outright, so a high factor thins as well as smooths. The canonical level stays
verbatim regardless. When coarse levels look blocky, lower this.

**`--no-cascade`.** By default each coarser level simplifies the next-finer
level's already-simplified output, tippecanoe-style, which is faster and
compounds detail reduction down the ladder. This flag simplifies each level from
the source instead, reproducing the pre-cascade output byte-for-byte at some
speed cost.

### Keeping tiny polygons visible

**`--collapse`.** Replaces a below-visibility polygon with a representative
point instead of dropping it, so a dense field of small parcels still reads as
presence at coarse zoom. It changes the geometry type, so a fill-only style
ignores the points; add a circle layer or use the square form below.

**`--collapse-square`.** Replaces the polygon with an area-dithered placeholder
square about one GSD across, staying a polygon so plain fill styles keep working.
The dithering makes a below-threshold polygon survive as a square with
probability proportional to its area, so dense blocks read denser than isolated
ones and aggregate area stays truthful. It is deterministic per feature.

**`--representation LO-HI:KIND`.** Assigns a disposition to a band of zooms, so
one PMTiles archive can show dots when zoomed out and full polygons up close.
`KIND` is `point`, `square`, or `geom`. This is the knob behind the demo's
dot-to-polygon handoff.

### Summarizing clustered points

**`--cluster`.** Lets the surviving point in a thinning cell absorb the others
rather than letting them vanish. The output gains a `point_count` column
recording how many source features each row represents, following the
tippecanoe and supercluster convention, so a graduated-dot style can size each
dot by its count. It applies to duplicating mode only.

**`--accumulate-attribute COL:OP`.** Aggregates a numeric column across the
absorbed points, where `OP` is `sum`, `max`, `min`, or `mean`. Repeat it per
column. It requires `--cluster`, since it summarizes exactly the points a
cluster absorbs.

### Connecting line networks

**`--no-coalesce-lines`.** The opt-out from line coalescing, which is on by
default. Coalescing chains touching same-class segments into single strokes
before the visibility gate runs, so a road or river that fragments into
sub-visible pieces survives as one long artery instead of a scatter of dashes.
The merged rows gain a `coalesced_count` column. Turn it off to reproduce
pre-coalescing output.

**`--coalesce-junction-angle` / `--coalesce-snap` / `--coalesce-max-level-rows`.**
The chaining knobs. The junction angle controls whether chains continue through
intersections, defaulting to 0 so junctions terminate chains and preserve network
topology. The snap tolerance bridges endpoint gaps within a GSD multiple. The
level-row ceiling is a memory guard that skips coalescing on datasets too large
to hold a level's candidate lines at once.
