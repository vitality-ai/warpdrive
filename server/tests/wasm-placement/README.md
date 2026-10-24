# Sandboxed user-defined placement policies (WASM) — prototype + safety suite

Pillar 3 of WarpDrive is "safely customizable": a customer defines how their
bytes are striped (a `StripePacker`), and the system explains the cost before
it goes live. This prototype shows how to run an **untrusted** packer safely in
a managed, multi-tenant service, and backs it with an **aggressive adversarial
test suite**. It is standalone (like `tests/chaos/jepsen-warpdrive`); it does
not change the server build.

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

## Explaining the cost

On load, the policy is run against a sample workload and its real storage
overhead vs MDS-optimal is reported — the same `overhead_pct` definition the
live overhead-threshold guard uses. Example: the `good_identity` policy (one
unit per stripe) is accepted but reported at **200% overhead**, so the customer
sees the cost before activating it.

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
