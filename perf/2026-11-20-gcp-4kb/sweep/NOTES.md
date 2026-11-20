# Saturation sweep notes

- Builds: #217 at 23ef9dd, #226 at b62e763. 4 KB objects, 120 s PUT then 120 s GET per point, one 2-vCPU client.
- Every point saturates the coordinator node (node0): CPU 92-99% busy, NVMe 17-40% busy, client CPU 7-12%.
  The limit is node0's 2 vCPUs, not the disks or the load generator.
- #226 PUT keeps rising to 1,225/s at 512 clients with no errors. #217 PUT falls to 452/s at 512 with 85,757 errors and a 10 s p99.
- #226 GET is 1,600-1,800/s. #217 GET is 1,950-2,470/s. #226 GET still costs more CPU per request than #217.
- The third variant (#226 with 256 HTTP workers) is left out: its servers never accepted connections
  ("connection refused" from the first request). The VMs were deleted before the cause was found.
