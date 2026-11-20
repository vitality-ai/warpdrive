# Core-count test notes

Question: does the coordinator go faster with 4 cores than with 2 at high client counts?

- One factor changes: node0 (n2-standard-4) runs pinned with `taskset` to cores 0-1 or 0-3. Same VM, build, disk and data path.
- #226 at b62e763. Nodes 1 and 2 are n2-standard-2. The client is n2-standard-4 (a 2-vCPU client hit 97% CPU on GET in the dry run).
- "cores busy" is node0's whole-VM CPU (4 vCPUs) x 4. With 2 cores pinned it can read slightly above 2.0 because the kernel handles network interrupts on the other cores.

Findings:

- **RS(2,1) is CPU-bound on node0.** With 2 cores it uses all of them. With 4 cores it uses 3.7, and throughput rises 32-41% (PUT 2,029 to 2,682, GET 3,081 to 4,283 at 128 clients). Doubling cores gives about 1.35x, so part of the per-request cost grows with cores (contention or thread hand-offs).
- **Single node is not CPU-bound.** It uses 1.6 of 2 cores and 1.9 of 4, NVMe is 30% busy, and throughput is flat or lower from 2 to 4 cores and from 128 to 512 clients. Something serializes requests (a lock or one-at-a-time sync on the write path). Separate bottleneck, not yet found.
- **512 clients add nothing over 128** in any setup. Every setup is at its ceiling by 128 clients.
