# Review of `warpdrive1_hipc2026.tex`

## Overall assessment

The submission has a clear systems idea: expose erasure-coded placement granularity as a per-object policy, then show that its costs vary by format and workload. The Parquet reproduction check and the dense parameter sweep are useful foundations. In its current form, however, the paper overstates what the simulator establishes and is not yet submission-ready.

## Must fix before submission

### 1. The source does not currently build from this directory

The LaTeX file references `pareto_both_formats.png` and `overhead_by_granularity.png`, but neither figure is present. Run `make_plots.py`, verify both outputs visually, and compile the final two-page PDF before submission.

### 2. “Round trips” is not a demonstrated latency metric

The simulator counts distinct touched bins. It does not model request concurrency, network paths, service time, decoding, or sequential dependencies. Several bin reads may execute in parallel, so the count cannot directly support statements such as “network-RTT-bound latency driver” or “costs 71.9 round trips.”

Rename the metric to “distinct block/bin reads” or “storage requests” unless a latency/concurrency model is added. Frame it as a latency proxy and state its limitations.

### 3. The Neon experiment does not model page reconstruction

Neon queries uniformly sample arbitrary layers by global selectivity. Actual page reconstruction follows a key/LSN-dependent image-plus-delta chain, with correlated layer accesses. Consequently, the experiment does not yet make the WarpDrive fragmented-versus-bundled case quantitative for Neon traffic.

Either:

- evaluate real or trace-derived reconstruction chains; or
- clearly label this as a synthetic sensitivity study and narrow the Neon claims.

### 4. The plotted curves are not necessarily Pareto frontiers

A dense sweep does not automatically form a Pareto frontier. Some measured points are dominated. For example, in the Parquet 50% results, batch 32 has both fewer reads and slightly lower byte amplification than batch 16.

Call each line a “granularity tradeoff curve” and optionally mark only nondominated points as the Pareto frontier.

### 5. The proposed HTTP header will not scale

`x-warpd-computable-units: [[offset,len],...]` can become very large. Thousands of units—as used in the Neon experiment—will exceed common HTTP and object-metadata limits. This weakens the claim that the mechanism is directly deployable.

Use or discuss a compact representation, a referenced sidecar manifest, or a separately uploaded policy object. Also explain validation, maximum unit count, and behavior for malformed or overlapping ranges.

### 6. The “same granularity” comparison needs more careful wording

Batch size 16 means one Parquet row group but an arbitrary group of 16 Neon layers. These are numerically equal settings, not semantically equivalent granularities. The 67× result is mathematically supported by the generated data, but the comparison alone does not prove that a universal policy fails.

Use “the same numeric batch size” and present the result as evidence that format structure strongly affects packing cost.

## Important revisions

### Methodology

- Define precisely what a “bin,” “storage unit,” “stripe,” and “round trip” means in the implementation.
- Explain why indivisible whole-object placement under RS(9,6) leaves eight data bins padded, producing 800% overhead. Readers may otherwise interpret this as an implementation artifact.
- State that the Parquet query model uniformly samples column chunks; it does not model realistic correlated projection and row-group pruning.
- Report dataset sizes in bytes, not only rows and unit counts.
- Explain what varies across the 25 trials: both regenerated synthetic data and query samples.
- Avoid treating the close 1.16% versus 1.2% result as full validation. It is a useful sanity check on a different synthetic dataset, not a reproduction of Fusion’s evaluation.

### Claims and presentation

- Replace “the winner flips with query selectivity.” The results show that tradeoff severity changes, but no objective function defines a winner.
- Avoid “an order of magnitude further apart” for the 1% comparison; specify exactly which ratio is meant.
- Clarify whether byte amplification values are excess percentages. For example, 101.9% means 2.019× bytes fetched, while 19,040% means 191.4×.
- State earlier that this is a simulator/standalone packing prototype, not a running object-store integration.
- Consider shortening the introduction. The explainability and agentic-systems framing currently receives more space than the concrete mechanism and threat-to-validity discussion.

### Citations and reproducibility

- Add the concrete WarpDrive URL/venue information already listed in `abstract_draft.md`.
- Cite a stable Neon source revision and exact file/constant rather than only the repository root.
- Cite the exact Fusion algorithm and experimental result location.
- Include a compact artifact/reproduction pointer if anonymity rules permit.
- Verify the exact HiPC 2026 anonymity, AI-disclosure, page-limit, and poster-submission requirements before finalizing the acknowledgment and author block.

## Suggested claim-safe wording

- “Distinct bins touched” instead of “round trips.”
- “Granularity tradeoff curve” instead of “Pareto frontier,” unless dominated points are removed.
- “Synthetic Neon-style layer population” instead of language implying measured Neon behavior.
- “The same numeric batch size produces a 67× difference in simulated packing overhead” instead of “the identical setting” without qualification.

## Strengths to preserve

- A simple, format-agnostic core abstraction.
- Explicit separation between finest logical query units and coarser storage grouping.
- Fixed EC parameters while sweeping granularity.
- Repeated trials with uncertainty bands.
- Clear disclosure that server integration and real Neon evaluation remain future work.

## Recommended decision

Major revision. The central idea is suitable for a poster, but the metric terminology, Neon workload model, frontier claim, header scalability, and missing build assets should be resolved before submission.
