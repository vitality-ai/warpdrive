import subprocess, statistics

def run_anyblob(nconn):
    cmd = [
        './AnyBlobBenchmark', 'minio', 'bandwidth',
        '-w', '10.128.0.4', '-p', '9710', '-b', 'realistic-delta-test', '-r', 'us-east-1',
        '-e', 'adminkey', '-k', '/home/nash/minio_secret.txt',
        '-f', 'layer-', '-n', '8', '-l', '8', '-c', '1', '-t', str(nconn), '-s', '65536',
    ]
    out = subprocess.run(cmd, cwd='/home/nash/cj/warpdrive/AnyBlob/example/benchmark/build',
                          capture_output=True, text=True).stdout
    line = out.strip().splitlines()[-1]
    fields = line.split(',')
    time_ms = float(fields[2])
    datasize = int(fields[14])
    return (datasize / 1e6) / (time_ms / 1000)

for nconn in [1, 2, 4, 8]:
    run_anyblob(nconn)  # warmup
    samples = [run_anyblob(nconn) for _ in range(10)]
    med = statistics.median(samples)
    print(f'nconn={nconn}  median={med:.1f} MB/s  range=[{min(samples):.1f}, {max(samples):.1f}]  '
          f'stdev={statistics.stdev(samples):.1f}  samples={[round(s,1) for s in samples]}')
