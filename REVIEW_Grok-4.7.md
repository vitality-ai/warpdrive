# Independent review of `warpdrive1_hipc2026.pdf`

Reviewer model: Grok 4.7  
Artifact reviewed: `warpdrive1_hipc2026.pdf` only (2 pages, US Letter, IEEE two-column).  
Figures and prose were read from the rendered pages.

## Recommendation

Major revision. The poster states a concrete question — how coarse should erasure-coded co-residency be, and does one alignment rule transfer from Parquet to Neon-style layers — but the printed argument overclaims what the two figures and the quoted numbers show. The layout also makes the evidence hard to check.

## What the poster contributes

The useful idea is a per-object batch size `b` on top of a format-agnostic stripe packer. The client supplies computable-unit boundaries in `x-warpd-computable-units` at PUT time, and the server packs those units with Fusion-style largest-item seeding and least-occupied-bin placement under RS(9,6). The evaluation sweeps that batch size on a synthetic TPC-H-shaped Parquet file and a synthetic population of 5,000 Neon-style layers, and it reports storage overhead, byte read amplification, and distinct bins touched.

Two observations in the text are worth keeping:

- At batch size 16, the poster reports 79.99% Parquet storage overhead and 1.20% Neon storage overhead.
- On Parquet, moving from the finest grouping to the whole file reduces distinct bins touched and increases byte amplification, at both 50% and 1% selectivity.

The closing paragraph is appropriately modest about server integration and real Neon traffic.

## Problems that should be fixed in the PDF

### 1. The abstract’s second claim does not match the body

The abstract says a dense sweep shows “storage overhead and round trips trading off in opposite directions” and “crossing over as query selectivity shifts.” Section II and Figure 2 say storage overhead is a packing property and is independent of query selectivity. The selectivity-dependent pair in the body is byte read amplification versus distinct bins touched.

The numbers printed for Finding 2 also stay in the same direction at both selectivities. At 50%, the finest point is 71.9 bins and 23.4% byte amplification, and the coarsest point is 1 bin and 101.9%. At 1%, the finest point is 2.0 bins and 114.8%, and the coarsest point is 1 bin and 19,040%. Finer grouping is better on bytes and coarser grouping is better on bins in both regimes. Selectivity changes the size of the gap. It does not, on the printed extremes, flip a winner. The poster never defines the objective that would make one point “correct.”

Rewrite item (ii) so it names byte amplification, and replace “the winner flips” with a statement about how the magnitude of the tradeoff changes.

### 2. “Round trips” is a bin count

The poster defines the metric as the number of distinct storage bins touched per query, then calls that count a network-RTT-bound latency and a “fully bundled round trip.” The PDF contains no request schedule, parallelism, stripe reconstruction, decode cost, or measured latency. A query that touches many bins may issue those reads concurrently. RS(9,6) can also require extra fragments when a bin is unavailable, which this count leaves out.

Label the axis and the prose as distinct bins touched, and keep latency language for a measurement the poster does not yet have.

### 3. “67× from the identical setting” compares unlike units

79.99 / 1.20 is about 67, so the arithmetic matches the printed medians. The same sentence says the Parquet point is exactly one row group and the Neon point is an arbitrary batch of 16 layers. Those are the same integer `b`, on different objects, with different size distributions. Figure 2 is still useful: Parquet overhead rises in large steps as batch size crosses structural boundaries, while Neon overhead stays near 1% across most of the sweep. That format dependence can be stated directly. “Identical granularity” is stronger than the setup supports.

The poster should also define “storage overhead vs. optimal” on the page. A reader cannot tell whether the denominator is the MDS size `(1 + m/k)` times the original bytes, or something else. The whole-object endpoint sits near the top of the log axis in Figure 2 and is never interpreted. If one indivisible object is placed into a width-9 stripe, most of that overhead is padding to the stripe, and the poster should say so.

### 4. The Neon panel is shown and then left unused

Finding 2’s quantitative comparison is Parquet only. Figure 1 still draws a full Neon curve for 1%, 10%, and 50% selectivity. The setup says the Neon population is sized from a 256 MiB target and a 3.86:1 delta-to-image ratio, then sampled as a selectivity fraction. The poster does not say what a Neon query selects: a random subset of layers, a key-range reconstruction, or an image-plus-delta chain. WarpDrive’s motivating case, as described in the introduction, is chain reconstruction. The printed experiment does not show that access pattern.

Either interpret the Neon curves under an explicitly synthetic access model, or narrow the Neon claim to the storage-overhead result in Figure 2.

### 5. The reproduction sentence validates less than it asserts

Column-chunk FAC at 1.16% overhead is close to the cited ~1.2% figure, which is a good sanity check. The poster then says this validates the generalized packer “before trusting anything built on top of it.” The check uses a synthetic 500,000-row file. Fusion’s cited number is from a real 10 GB file. Matching one overhead percentage does not validate the Neon grouping rule, the read-amplification model, or the header mechanism.

### 6. “GCP-hosted prototype” is not evidenced on the page

That phrase appears in the abstract. The results section describes synthetic inputs, a packer, medians over 25 trials, and 300-query-style selectivity sweeps. No region, API, stored object, or measured GET latency appears. The conclusion correctly calls the work a standalone prototype whose server integration is ongoing. The abstract should use that description.

### 7. The proposed header is not credible at the poster’s own scale

The mechanism is an inline PUT header, `x-warpd-computable-units: [[offset,len],...]`. The Neon experiment uses 5,000 units. The poster gives no bound on header size, no validation rule for overlapping or uncovered ranges, and no alternative when the boundary list exceeds HTTP or object-metadata limits. A compact manifest, or a separately stored policy object, needs at least a sentence if this is the deployment claim.

## Figure and page layout

The PDF fits the two-page limit, and the section order is readable: motivation, mechanism, results, conclusion. Page 1 is continuous prose. Page 2 places both figures in the right column after Finding 2 finishes. That reading order works, but the figures are the evidence and they are too small to inspect.

- Figure 1 puts two log-log plots side by side inside one column. Axis titles, the shared selectivity legend, and the “finest” / “coarsest” tags are at a size where endpoint labels collide. The [25th, 75th] bands mentioned in the setup are not distinguishable in the printed figure.
- The plot titles repeat the caption, which spends the already small area on duplicate words.
- Three selectivity colors (purple, pink, orange) on a cream background will merge if the poster is printed or photocopied in grayscale. Distinct line styles or markers would survive that.
- Figure 2’s log y-axis runs to about 10³ because the whole-object point is far above the rest. That scale compresses the 1%–80% region where Finding 1 actually lives. A callout, a broken axis, or annotated medians at batch 1, 16, and the whole object would let a poster reader see the 67× comparison without trusting the prose alone.
- Intermediate batch sizes are drawn, but only the endpoints are labeled, so a reader cannot map a point in Figure 1 back to a batch size in Figure 2.

For a poster session, the first page also spends most of its space restating why object storage is hard to explain. The mechanism — what a computable unit is, what `b`, `k`, and `m` change, and what one query reads — arrives late and has no worked example.

## Smaller issues

- Report the interquartile ranges next to 79.99%, 1.20%, 71.9, and 19,040%, since the setup says every point has them.
- Say what is randomized across the 25 trials. Overhead is described as independent of the query sample, so readers need to know whether each trial rebuilds the file.
- Define selectivity: fraction of column chunks, fraction of layers, or fraction of bytes, chosen uniformly or with locality.
- “An order of magnitude further apart” needs the ratio it refers to. The printed byte figures go from about 4× apart at 50% (101.9 vs. 23.4) to about 166× apart at 1% (19,040 vs. 114.8). The bin counts do not move by an order of magnitude between those extremes.
- State that 19,040% byte amplification means roughly 191× bytes fetched, not a 19,040× multiplier. Percent overhead at that magnitude is easy to misread on a poster.
- Citation [2] is an anonymous workshop paper, and citation [3] is a repository URL with a filename and no revision. Pin the Neon constant to a commit if the 256 MiB target is load-bearing.
- The acknowledgment names a specific assistant and says it follows the HiPC 2026 AI-use policy. Keep it if the call requires that disclosure; it does not affect the technical claims.

## Questions for the authors

1. What is the exact formula for storage overhead versus optimal, and why does the coarsest point sit so high on Figure 2?
2. For one Parquet query at 1% selectivity, which column chunks are chosen, and which bins does batch size 1 touch versus batch size 160?
3. What access pattern produces the Neon curves in Figure 1?
4. Which points on each curve are nondominated, and which objective decides the preferred batch size?
5. Where does the client-supplied boundary list live once an object has thousands of units?

## Strengths worth keeping

The poster names its limits: synthetic data, a standalone prototype, and real Neon traffic still to come. It holds the code parameters at RS(9,6) while sweeping granularity, and it separates a packing metric from query metrics. The Parquet 1.16% check is a sensible anchor before the broader sweep. Figure 2, once annotated and explained, is the clearest evidence that one numeric batch size has very different packing cost on these two layouts.
