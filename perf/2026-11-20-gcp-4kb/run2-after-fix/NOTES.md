# Run 2 notes

- Builds: #217 at 23ef9dd, #226 at b62e763 (O(1) blob cache, HTTP workers at least 32).
- **#226 + inline 8 KB is not a valid result.** About 6 minutes into its PUT phase, node0 (the endpoint) started answering with
  "connection reset by peer" (210 PUT errors), and the GET phase ran at 1.5/s with a 20 s p99. The node memory
  sampler had a bash bug in this run and recorded nothing, so memory for that configuration is missing, not zero.
  The kernel log on node0 shows no out-of-memory kill. The cause is being reproduced with server logs kept.
- Memory samples exist for the first three configurations only (the old sampler); the last uses the fixed sampler format but recorded nothing.
