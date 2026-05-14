# dataseed

Plant a seed, grow a dataset.

`dataseed` is a CLI that reads a tiny `.dataseed` file and produces a large,
realistic-looking dataset for testing — fixtures for databases, ETL
pipelines, demo data, anywhere you'd otherwise hand-roll fake data.

## 30-second quickstart

```sh
# build
cargo build --release

# look at an example
cat examples/trees.dataseed

# generate 10,000 rows of deterministic SQL
./target/release/dataseed plant examples/trees.dataseed --seed 42

# the same seed always produces byte-identical output
./target/release/dataseed plant examples/trees.dataseed --seed 42 \
  | shasum   # same hash every time

# write to a file, override the row count
./target/release/dataseed plant examples/trees.dataseed --seed 42 \
  --count 1000 -o trees.sql

# list every generator function (also: --json for machine consumption)
./target/release/dataseed functions

# validate a file without generating data
./target/release/dataseed lint examples/trees.dataseed
```

## A `.dataseed` file

Two equivalent shapes — single table or multi-table.

**Single table** (Phase 1/2 form, still fully supported):

```
output: sql
table: trees

schema {
  id:      sequence
  species: randomChoice("oak", "birch", "pine", "maple")
  height:  randomRealNumber(1.0, 45.0, decimals: 2)
  planted: randomDate("1990-01-01", "2024-12-31")
  alive:   randomBool(weight: 0.85)
}

generate 10000
```

**Multi-table** (Phase 3 form) — each `table NAME { ... }` block is
self-contained, and `generate NAME: N` says how many rows of each:

```
output: sql

table users {
  id:   sequence
  name: randomName()
}

table orders {
  id:      sequence
  user_id: ref(users.id)
  total:   randomRealNumber(5, 500, decimals: 2)
}

generate users: 1000
generate orders: 10000
```

Top-level directives:

| directive            | example                  | notes                                       |
|----------------------|--------------------------|---------------------------------------------|
| `output:`            | `output: sql`            | `sql`, `postgis`, or `json`                 |
| `table NAME { ... }` | `table users { ... }`    | one or more; fields go directly inside      |
| `table: NAME`        | `table: trees`           | legacy form — pair with a `schema { ... }`  |
| `schema { ... }`     | see above                | legacy form — bundles fields for `table:`   |
| `generate NAME: N`   | `generate users: 1000`   | per-table row count (multi-table)           |
| `generate N`         | `generate 10000`         | bare form — single-table files only         |

Both forms can coexist in one file (helpful for migration). Comments
start with `#` and run to end of line.

## Built-in generators

Run `dataseed functions` for a formatted list, or `dataseed functions --json`
for a stable machine-readable schema designed for LLM tool use.

| name                | example                                          |
|---------------------|--------------------------------------------------|
| `sequence`          | `sequence  # or sequence(start: 1000)`           |
| `randomInt`         | `randomInt(1, 100)`                              |
| `randomRealNumber`  | `randomRealNumber(1.0, 45.0, decimals: 2)`       |
| `randomBool`        | `randomBool(weight: 0.85)`                       |
| `randomChoice`      | `randomChoice("oak", "birch", "pine")`           |
| `randomWord`        | `randomWord()`                                   |
| `randomName`        | `randomName()`                                   |
| `randomEmail`       | `randomEmail()`                                  |
| `randomDate`        | `randomDate("1990-01-01", "2024-12-31")`         |
| `randomUuid`        | `randomUuid()`                                   |
| `randomPoint`       | `randomPoint(bbox: [3.3, 50.7, 7.2, 53.5])`      |
| `randomPointNear`   | `randomPointNear(center: [5.12, 52.37], radius_m: 1000)` |
| `randomLineString`  | `randomLineString(bbox: [...], segments: 8, jitter: 0.4)` |
| `randomPolygon`     | `randomPolygon(bbox: [...], vertices: 6, irregularity: 0.3)` |
| `randomBbox`        | `randomBbox(within: [...], min_size_deg: 0.01, max_size_deg: 0.5)` |
| `ref`               | `ref(users.id)` — uniformly random value drawn (with replacement) from a column in another table |

## Geospatial output

Three output modes, chosen via the `output:` directive:

| mode       | non-geometry cells   | geometry cells                                   |
|------------|----------------------|--------------------------------------------------|
| `sql`      | standard SQL literal | WKT string in single quotes — `'POINT(...)'`     |
| `postgis`  | same as `sql`        | `ST_GeomFromText('POINT(...)', 4326)`            |
| `json`     | JSON-native          | inline GeoJSON object — `{"type": "Point", …}`   |

Pick `postgis` when you're loading into a PostGIS-enabled Postgres
(`psql -f out.sql`). Pick `sql` if you want WKT strings in any other
database, or if you want to call `ST_GeomFromText(...)` yourself in the
ingest pipeline. Pick `json` for browser-side mapping libraries
(MapLibre / Leaflet) — the inline GeoJSON objects drop straight into a
FeatureCollection with `jq`.

All coordinates are WGS84 (longitude, latitude) and rounded to 7 decimal
places (≈ 1 cm at the equator) at output time. The underlying f64 values
keep full precision in memory; rounding is purely a presentation choice.

Three example files cover the typical shapes:

- `examples/fields.dataseed` — agricultural polygons, `output: postgis`
- `examples/sensor_locations.dataseed` — point cloud, `output: sql`
- `examples/bike_routes.dataseed` — polylines, `output: json`

## Relations

Multi-table files can reference another table's column with `ref(T.C)`.
Selection is **uniform with replacement** — every value in `T.C` is
equally likely to appear; the same parent value may be picked many times.

```
table users {
  id:   sequence
  name: randomName()
}

table orders {
  id:      sequence
  user_id: ref(users.id)
  total:   randomRealNumber(5, 500, decimals: 2)
}

generate users: 1000
generate orders: 10000
```

Generation order is decided by topological sort over the ref graph
(`users` before `orders` because `orders` refs `users`). Ties between
independent tables are broken alphabetically, so the order is stable
across edits that don't change the ref graph. `dataseed lint` reports
the order for any multi-table file:

```
$ dataseed lint examples/fleet.dataseed
ok: examples/fleet.dataseed
  output: postgis
  tables: drivers (30), vehicles (25), trips (200)
  generation order: drivers → vehicles → trips
```

### What's caught at lint time

| problem                                               | error                       |
|-------------------------------------------------------|-----------------------------|
| `ref(missing.id)` — table doesn't exist               | `UndeclaredRefTable`        |
| `ref(users.unknown)` — column doesn't exist in table  | `UndeclaredRefColumn`       |
| `ref(self.something)` — Phase 3 forbids self-refs     | `SelfReference`             |
| Two tables ref each other (or 3-cycle, etc.)          | `CyclicReference` — lists every edge in the cycle |
| Declared table has no `generate` directive            | `MissingGenerate`           |
| `generate ghost: N` for an undeclared table           | `GenerateForUnknownTable`   |

### Variable child counts: `per_parent`

A `ref()` may take `per_parent: N..M` to declare that each parent row owns
between N and M children. The child table's row count is **derived** —
omit `generate posts: K` and dataseed sums the per-parent draws.

````
table posts {
  id:        sequence
  author_id: ref(users.id, per_parent: 0..20)
}
generate users: 50
````

Constraints:
- Only one field per child table may use `per_parent`.
- A child driven by per_parent must not have an explicit `generate`
  directive — the count comes from the parent.
- The parent's column must exist (typos are surfaced as
  `UndeclaredRefTable` / `UndeclaredRefColumn` exactly as for a plain
  `ref()`).

See `examples/blog.dataseed` for a realistic shape.

### CLI flags for multi-table files

```
dataseed plant shop.dataseed --seed 42                       # all tables
dataseed plant shop.dataseed --seed 42 --table orders        # only orders' rows emitted; users still pooled for refs
dataseed plant shop.dataseed --count users=500 --count orders=2000   # per-table overrides
dataseed plant shop.dataseed --count 100                     # bare form — only valid in single-table files
```

### Single-table output stability

Single-table files emit the same bytes they did in Phase 1/2 — the
`-- Table: NAME (N rows)` header and the JSON top-level object are
only added when a file declares more than one table. **Single-table
output is stable from Phase 1 onward.**

## Type system

Phase 2 added `array<T>` (used by geospatial generators for `bbox`,
`center`, etc.); Phase 3 added `column_reference` (used by `ref()`).
Arrays can carry a fixed-length constraint exposed in the catalog:

```json
{
  "name": "bbox",
  "type": "array<number>",
  "length": 4,
  "required": true
}
```

The `ref` generator's return type is the documented sentinel
`"depends_on_target"` — its concrete type is whatever type the referenced
column has, decided at semantic-check time.

## Determinism

`--seed N` makes generation byte-for-byte reproducible across runs and —
by design — across platforms. We use `ChaCha8Rng` (`rand_chacha`) rather
than the default `StdRng` because `StdRng`'s algorithm is explicitly
allowed to change between `rand` minor versions, which would silently
break the reproducibility guarantee. ChaCha8's state and stream are
platform-independent: the seed fully determines the byte stream, and
every generator consumes from it in declaration order.

The endianness-sensitive ingredients we touch (UUID byte layout, date
arithmetic, `f64` formatting) all use platform-neutral code paths:
`uuid::Uuid::from_bytes` reads bytes in big-endian order regardless of
host, `chrono::NaiveDate` arithmetic is integer-only, and Rust's `f64`
`Display` is Ryu-based and identical across targets.

For geospatial generators specifically: `randomPointNear` needs one
`cos(latitude)` call at bind time for the equirectangular metres-to-degrees
conversion. We use [`libm::cos`](https://crates.io/crates/libm) (pure-Rust
software libm) instead of `f64::cos` because the platform libm is allowed
to differ in last-mantissa-bit results between macOS/Linux/Windows.
`libm` is bit-identical everywhere. The other four geospatial generators
are trig-free.

**Verified locally** on Darwin arm64 (M-series Mac). Reference SHAs for
the three CLI invocations below:

| invocation                                                                  | sha256                                                             |
|-----------------------------------------------------------------------------|--------------------------------------------------------------------|
| `dataseed plant examples/trees.dataseed  --seed 42  --count 100` (Phase 1)  | `066138bad6ab881056c4a634d0dcd6d09dfef963cc9be86968fbe1dc1eb3bf96` |
| `dataseed plant examples/users.dataseed  --seed 7   --count 100` (Phase 1)  | `603b1f132073f4cbaaec4d8add40095f90c808704105410f6a746d3e221bc73e` |
| `dataseed plant examples/orders.dataseed --seed 123 --count 100` (Phase 1)  | `eae65906e81077b9f1534c6d76b8691027afe100f9338679004fa97d94646c02` |
| `dataseed plant examples/shop.dataseed   --seed 42`               (Phase 3) | `f13feb9a2a3275f4faaca6e8186a69ce169100023af5bf2ab34d4783f42b22f3` |
| `dataseed plant examples/fleet.dataseed  --seed 42`               (Phase 3) | `2eacca59c9eee642ed3ce542e08372bd44b3534bf1b28bdfc2d3801d631255f5` |
| `dataseed plant examples/blog.dataseed   --seed 42`               (Phase 4) | `881703fba4457302d84338829f41fc816927d575f0513f351f9abee37dc0cda1` |

If you build on Linux x86_64, Windows, or another target and these
hashes don't match, that's a bug — please open an issue with your
platform and a short reproduction.

Without `--seed`, the CLI picks a fresh entropy-based seed and prints it
to stderr so you can reproduce a one-off run later.

## Error messages

Errors are designed to be actionable, with line/column references and
"did you mean?" hints for typo'd function names:

```
$ dataseed lint examples/typo.dataseed
Error: unknown function `randomNam` at line 4, column 12
Hint: did you mean `randomName`?
```

For duplicate top-level directives, both line numbers are reported so you
can pick which to remove.

## Scope

Phases 1, 2, 3, and 4 (in progress: `per_parent` for variable child counts) are shipped: parser + AST, semantic analysis,
16 generators (10 scalar + 5 geospatial + `ref`), multi-table files
with topological generation order and cycle detection, SQL / PostGIS /
JSON output, deterministic generation, machine-readable `--json` catalog.

Out of scope (planned for later phases): distribution skew on refs
(zipf/etc.), correlated refs (`order.created_at > user.signup_date`),
nested JSON output, spatial relations ("orders within 1km of warehouse"),
custom CRS beyond WGS84, custom templates, streaming output, parallel
generation, and CSV / XML / Parquet output formats.

The bundled wordlist (`src/generators/data/words.txt`) and name lists are
intentionally modest — enough to feel realistic for fixtures. Swap them
for SCOWL or any other list if you need broader vocabulary.

## License

MIT.
