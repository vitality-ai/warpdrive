# Placement policies as submitted compute units (WASM) — prototype + safety suite

Pillar 3 of WarpDrive is "safely customizable." The model this prototype
demonstrates: **at bucket creation, a customer submits a tiny compute unit** — a
WASM module implementing `StripePacker` — that decides how that bucket's objects
are striped. Before the bucket is accepted, WarpDrive runs the module in a
sandbox against a sample workload and returns a full **policy report** that
explains, in detail, what the customer is signing up for. A submission that
fails any safety or structural check is rejected and the bucket is refused.

This prototype runs that untrusted compute unit safely in a managed,
multi-tenant setting and backs it with an **aggressive adversarial test suite**.
It is standalone (like `tests/chaos/jepsen-warpdrive`); it does not change the
server build.

## Bucket-creation flow

```
CreateBucket { name, placement_policy: <wasm compute unit>, overhead_threshold }
   -> sandboxed run on a sample workload
   -> POLICY REPORT (below) returned to the customer
   -> bucket created with the policy bound, OR refused with the report
```
`WasmPacker::explain(bucket, k, m, sizes) -> PolicyReport` is the entry point a
`CreateBucket` handler would call.

## The safety model

A user policy runs as a WASM module (wasmtime), wrapped behind the existing
`StripePacker` contract. The guarantees, and how each is enforced:

| Threat | Enforcement |
|---|---|
| Reaching the host (syscalls, clock, RNG, network, fs, cross-tenant data) | **No imports provided** — any module that imports anything fails to instantiate |
| Infinite loop / CPU burn | **Fuel budget** (+ epoch wall-clock backstop) — traps |
| Memory bomb | **`StoreLimits` memory cap** — growth denied, OOB access traps |
| Huge / unbounded output | **Output caps** (≤ n+1 stripes, counts bounded) + every read bounds-checked |
| Fabricated unit ids | **Index-based ABI** — a module may only reference the units it was handed |
| Split / dropped / duplicated unit, over-capacity bin | **Structural validation on every pack** — each unit placed exactly once, no bin over capacity |
| Nondeterminism (breaks the cost SLA) | No clock/RNG imports ⇒ deterministic ⇒ simulated cost == production cost |

The load-bearing rule: the safety boundary is the **cheap per-invocation
structural validation**, not a one-time pre-activation simulation. Simulation
previews cost; validation (run every pack) is what prevents corruption. Any
failure ⇒ `Rejected` ⇒ caller degrades to the plain built-in path. A rejected
policy never shapes user data.

## The policy report — "explain a lot more"

The report is not a single number. For an accepted submission it covers:

- **Storage cost** — overhead vs MDS-optimal, stripe count, physical bytes, and
  padding waste; plus **how it compares to the built-in default policy**, so the
  customer sees the price of customizing vs not.
- **Durability / blast radius** — how many objects share a stripe (max and avg),
  i.e. how many are lost *together* if a stripe is lost. This is the real
  tradeoff: more objects per stripe is cheaper but widens correlated-loss blast
  radius.
- **Policy compute cost** — the submitted unit runs on **every PUT**, so its own
  cost is metered and shown: fuel (≈instructions), wall time, peak sandbox
  memory, and a determinism verdict (required, or the quoted cost wouldn't hold).
- **Sample placement** — a concrete rendering of how the sample workload got
  packed, so the behavior is legible, not abstract.

Example (`good_identity`, one object per stripe): ACCEPTED, **200% overhead
(+195.6% vs the default's 4.4%)**, blast radius **1 object** — i.e. perfect
isolation bought at a large storage premium. That tradeoff is exactly what the
report makes explicit before the bucket is created.

## Running

```bash
cd server/tests/wasm-placement
cargo run           # runs the suite against the checked-in fixtures/*.wasm
```
No wasm toolchain is needed to run it — the adversarial modules are checked in
as `fixtures/*.wasm`. To regenerate them from source:
```bash
rustup target add wasm32-unknown-unknown
./build_modules.sh  # modules/*.rs -> fixtures/*.wasm
```

Current result: **8/8 PASS** — every hostile module contained for the right
reason; the one valid policy accepted with its cost explained.

## Experiments: explainability + agent scale (`cargo run --bin sweep`)

A deterministic sweep of **3 policies × 4 workloads × 4 unit-counts = 48
experiments** backs two claims with numbers.

**Explainability — one report, one frontier.** For `uniform`, n=256, RS(3,2):

| policy | overhead | blast radius (objects lost together) | reading |
|---|---|---|---|
| `good_identity` | 200.0% | 1 | max isolation |
| `good_pairs` | 50.0% | 2 | balanced |
| `good_packall` | 0.8% | 256 | min overhead |

Every policy lands on the *same* quantitative cost↔durability frontier, so a
human or an agent picks a point deliberately instead of guessing. (The report
also shows each vs the built-in default, the per-PUT compute cost, and a sample
placement — see above.)

**Agent scale — the policy is a per-PUT compute unit, and its cost is bounded.**
`pack` runs on every PUT over that object's units, so per-PUT cost must stay low
and linear. Measured (workload=`skewed`):

| policy | n=16 | n=64 | n=256 | n=1024 | fuel/unit |
|---|---|---|---|---|---|
| `good_identity` | 885 | 3381 | 13365 | 53301 | ~52 (flat) |
| `good_pairs` | 798 | 3006 | 11838 | 47166 | ~46 (flat) |
| `good_packall` | 1377 | 4737 | 18177 | 71937 | ~70 (flat) |

Fuel is O(n) with a flat per-unit cost; wall time stays **sub-millisecond even
at 1024 units/object**, so at agent PUT rates the policy is a negligible,
bounded slice of each write. The whole 48-experiment space ran in **~144 ms**
and is **reproducible** (same seed → identical overhead and fuel) — so an agent
can explore the policy space and automate the choice.

## ABI

Input (at a module-allocated ptr): `[u32 k][u32 n][n × u32 size]`.
`pack(in_ptr, in_len) -> out_ptr`. Output at `out_ptr`:
`[u32 num_stripes]` then per stripe `[u32 capacity][u32 num_bins]` then per bin
`[u32 num_units][num_units × u32 unit_index]`. Units are referenced by index,
never by id, so a module cannot invent or rename a unit.

## Finding for the real server (prerequisite for shipping user packers)

`coordinator.rs` (`put_object_plain`, ~L409) gates a packer's output on **only**
the overhead threshold — there is no structural validation:

```rust
let stripes = packer.pack(k, &units);
let overhead = overhead_pct(k, m, &units, &stripes);
if overhead <= config.overhead_threshold_pct { /* write it */ }
```

Safe for the built-in `FacPacker` (correct by construction, unit-tested), but a
user-defined packer that **drops a unit** (→ silent data loss, PUT still 200) or
emits an **over-capacity bin** (which *lowers* overhead, so it *passes* the gate
→ corruption) would be accepted. The fix is to run `validate()` (this crate's
`lib.rs`) on every pack output before `put_object_content_dependent`, degrading
to plain on any violation — defense that also protects the built-in path. This
should land before any user-defined packer is enabled.
