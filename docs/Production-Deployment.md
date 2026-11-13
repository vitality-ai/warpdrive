# Production Deployment

This is guidance and a starting point, not a tested, ready-to-run deployment
— unlike the rest of this repo's docs, nothing on this page has been
verified against a real production cluster. Treat the YAML below as a
sketch to adapt and validate against your own infrastructure, not something
to apply as-is.

Docker Compose (see [setup.md](setup.md)) is a local dev/demo convenience
only — it has no notion of multiple hosts, which a real WarpDrive cluster
needs. For production, run WarpDrive the way you'd run any other stateful,
peer-aware clustered service: one process per host (or per pod), each told
the full set of peers up front.

## Why a StatefulSet, not a Deployment

WarpDrive's cluster engine needs two things a plain Kubernetes `Deployment`
doesn't give you:

1. **Stable network identity per instance.** `WARPDRIVE_PEERS` is a fixed
   list of addresses, and every node needs to be reachable at the *same*
   address across restarts/rescheduling — a `Deployment`'s pods get new,
   unstable IPs each time. A `StatefulSet` + a **headless Service**
   (`clusterIP: None`) gives each pod a stable DNS name instead
   (`warpdrive-0.warpdrive.<namespace>.svc.cluster.local`,
   `warpdrive-1...`, ...), which is what `WARPDRIVE_SELF_ADDR` and
   `WARPDRIVE_PEERS` should actually point at.
2. **Stable, per-instance storage.** Each node has its own shard data and
   location-pin logs — not shared, not stateless. A `StatefulSet`'s
   `volumeClaimTemplates` gives each ordinal its own `PersistentVolumeClaim`
   that follows it across rescheduling, the same role
   `scripts/run_local_cluster.sh`'s per-node directories play locally.

This sketch uses a **fixed-size** `StatefulSet` (`replicas: 5`, matching the
project's own `RS(3,2)` default) with a statically-computed `WARPDRIVE_PEERS`
list, not the dynamic `WARPDRIVE_JOIN_VIA`/`WARPDRIVE_SELF_ADDR` join flow —
simpler to reason about and closer to how this project's own tests run a
cluster. Scaling the replica count after the fact means recomputing and
rolling out the new peer list to every pod; there's no live-rebalancing
story here, same as the rest of the project (see
`docs/Distributed-Engine-Plan.md`: "no automatic rebalancing of already
-placed data").

## Spreading nodes across real failure domains

`RS(3,2)` tolerating 2 node failures only means something if those 2 nodes
can actually fail independently. Without constraints, Kubernetes may
schedule several WarpDrive pods onto the *same* underlying node or
availability zone, in which case losing that one node could take out more
shards than the erasure code is rated for. Use **pod anti-affinity** or
**topology spread constraints** to keep replicas apart:

```yaml
topologySpreadConstraints:
  - maxSkew: 1
    topologyKey: kubernetes.io/hostname
    whenUnsatisfiable: DoNotSchedule
    labelSelector:
      matchLabels:
        app: warpdrive
  # Repeat with topologyKey: topology.kubernetes.io/zone if you have
  # multiple availability zones and want zone-level fault tolerance too.
```

## Sketch

```yaml
apiVersion: v1
kind: Secret
metadata:
  name: warpdrive-admin
stringData:
  access-key: CHANGE-ME
  secret-key: CHANGE-ME-TOO
---
apiVersion: v1
kind: Service
metadata:
  name: warpdrive          # headless -- peer discovery, not client traffic
spec:
  clusterIP: None
  selector:
    app: warpdrive
  ports:
    - { name: s3,   port: 9710 }
    - { name: grpc, port: 10710 }  # shard transport (WARPDRIVE_PEER_TRANSPORT=grpc, the default)
    - { name: tcp,  port: 11710 }  # raw-TCP shard transport, if selected instead
---
apiVersion: v1
kind: Service
metadata:
  name: warpdrive-s3        # client-facing: any pod can serve any request
spec:
  selector:
    app: warpdrive
  ports:
    - { name: s3, port: 9710 }
  type: ClusterIP            # or LoadBalancer, depending on how clients reach it
---
apiVersion: apps/v1
kind: StatefulSet
metadata:
  name: warpdrive
spec:
  serviceName: warpdrive     # must match the headless Service above
  replicas: 5                # RS(3,2): k=3 data + m=2 parity
  selector:
    matchLabels: { app: warpdrive }
  template:
    metadata:
      labels: { app: warpdrive }
    spec:
      topologySpreadConstraints:
        - maxSkew: 1
          topologyKey: kubernetes.io/hostname
          whenUnsatisfiable: DoNotSchedule
          labelSelector:
            matchLabels: { app: warpdrive }
      containers:
        - name: warpdrive
          image: tejascj/warpdrive:latest   # multi-arch (amd64, arm64), built from this repo's Dockerfile
          ports:
            - { containerPort: 9710 }
            - { containerPort: 10710 }
            - { containerPort: 11710 }
          env:
            - name: WARPDRIVE_ADMIN_ACCESS_KEY
              valueFrom: { secretKeyRef: { name: warpdrive-admin, key: access-key } }
            - name: WARPDRIVE_ADMIN_SECRET_KEY
              valueFrom: { secretKeyRef: { name: warpdrive-admin, key: secret-key } }
            - name: WARPDRIVE_RS_K
              value: "3"
            - name: WARPDRIVE_RS_M
              value: "2"
            # Each pod's own stable name -- $(HOSTNAME) is the ordinal-suffixed
            # pod name a StatefulSet gives you (warpdrive-0, warpdrive-1, ...).
            - name: WARPDRIVE_SELF_ADDR
              value: "http://$(HOSTNAME).warpdrive:9710"
            # Static, since replicas is fixed above -- all 5 ordinals,
            # same list on every pod.
            - name: WARPDRIVE_PEERS
              value: >-
                http://warpdrive-0.warpdrive:9710,
                http://warpdrive-1.warpdrive:9710,
                http://warpdrive-2.warpdrive:9710,
                http://warpdrive-3.warpdrive:9710,
                http://warpdrive-4.warpdrive:9710
            # These four default to paths relative to the container's
            # CWD (/server), outside the /data mount below -- on pod
            # replacement that would lose the location/shard-metadata
            # logs while the actual shard bytes under /data/storage
            # survive, leaving persisted objects with no pin to find
            # them by. Same fix as scripts/docker_cluster_entrypoint.sh.
            - { name: WARPDRIVE_LOCATION_LOG,         value: /data/cluster_location.log }
            - { name: WARPDRIVE_CONTENT_LOCATION_LOG, value: /data/cluster_content_location.log }
            - { name: WARPDRIVE_BUCKET_CONFIG_LOG,    value: /data/cluster_bucket_config.log }
            - { name: WARPDRIVE_SHARD_META_LOG,       value: /data/shard_meta.log }
          volumeMounts:
            - { name: data, mountPath: /data }
          readinessProbe:
            httpGet: { path: /s3, port: 9710 }
            initialDelaySeconds: 5
          livenessProbe:
            httpGet: { path: /s3, port: 9710 }
            initialDelaySeconds: 10
  volumeClaimTemplates:
    - metadata: { name: data }
      spec:
        accessModes: ["ReadWriteOnce"]
        resources:
          requests:
            storage: 50Gi   # size for your own workload -- not a verified number
```

## What's deliberately left for you to decide

- **Resource requests/limits** (CPU/memory per pod) — this depends on your
  workload, not something this project has load-tested on Kubernetes
  specifically to hand you a number.
- **Rolling upgrades**: a `StatefulSet`'s default update strategy restarts
  one ordinal at a time. `RS(3,2)` tolerating 2 nodes down means losing 1
  pod mid-rollout is survivable by the same quorum math used everywhere
  else in this project (`required_write_acks` in `coordinator.rs`) — but
  this hasn't been exercised as a real rolling-upgrade test here.
  `maxUnavailable` on a rolling update, and readiness gating, should both
  respect however many node losses your chosen `RS(k,m)` actually tolerates.
- **Autoscaling the node count** isn't addressed here at all — this project
  has no live-rebalancing story yet (see the note above), so growing a
  cluster today is an explicit, deliberate operation (update the replica
  count and every pod's `WARPDRIVE_PEERS`, not a HorizontalPodAutoscaler).
- **TLS termination** — WarpDrive itself doesn't terminate TLS (see
  `docs/user_guide.md`'s note on plain HTTP); put a reverse proxy or your
  cloud provider's load balancer in front of `warpdrive-s3` for that.
