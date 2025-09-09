# TLA+ Specifications for WarpDrive

## Overview

This directory contains TLA+ specifications for the WarpDrive storage system. We start with single-node specifications to establish correctness properties before scaling to distributed systems.

## Why TLA+ for WarpDrive?

### 1. **Current System Benefits**
- **Formal Verification**: Prove correctness of core storage operations (PUT, GET, DELETE, UPDATE)
- **Invariant Checking**: Ensure data consistency, user isolation, and no duplicate keys
- **Edge Case Discovery**: Find race conditions and unexpected behaviors in concurrent operations
- **Documentation**: Formal specification serves as living documentation

### 2. **Future Distributed System Foundation**
- **Consistency Models**: Define what "correct" means for distributed storage
- **Replication Protocols**: Model eventual consistency, strong consistency, or CRDTs
- **Partition Tolerance**: Handle network partitions and node failures
- **Conflict Resolution**: Define how to resolve concurrent updates

## Files

### `WarpDrive_SingleNode.tla`
Models the current single-node system with:
- **Operations**: PUT, GET, DELETE, UPDATE, UPDATE_KEY
- **State**: Storage data and metadata tracking
- **Invariants**: Data consistency, user isolation, no duplicates
- **Properties**: consistent behavior

### `WarpDrive_SingleNode.cfg`
TLC configuration for model checking:
- Small state space for verification
- Invariant checking enabled
- Symmetry reduction for efficiency

## Key Invariants Modeled

1. **DataConsistency**: Storage and metadata are always in sync
2. **NoDuplicateKeys**: Each user can't have duplicate keys
3. **UserIsolation**: Users can't access each other's data
4. **Consistent**: System reaches consistent state

## Running the Model

```bash
# Install TLA+ tools
# Download from: https://github.com/tlaplus/tlaplus/releases

# Run model checker
java -jar tla2tools.jar WarpDrive_SingleNode.tla
```

## Future Extensions

### Distributed System Models
1. **Multi-Node Replication**: Model data replication across nodes
2. **Consensus Protocols**: Raft, PBFT for leader election
3. **Network Partitions**: Handle split-brain scenarios
4. **Eventual Consistency**: Model AP systems with conflict resolution

### Advanced Properties
1. **Linearizability**: Strong consistency guarantees
2. **Causal Consistency**: Weaker but practical consistency
3. **CRDTs**: Conflict-free replicated data types
4. **Sharding**: Horizontal partitioning strategies

## Benefits for WarpDrive Evolution

1. **Confidence**: Prove correctness before implementation
2. **Design Space Exploration**: Compare different consistency models
3. **Bug Prevention**: Catch design flaws early
4. **Team Communication**: Formal specs reduce ambiguity
5. **Compliance**: Meet formal verification requirements

## Next Steps

1. **Run Current Model**: Verify single-node properties
2. **Add More Operations**: Model APPEND and complex workflows  
3. **Concurrency**: Add interleaving of operations
4. **Distributed Model**: Start with simple 2-node replication
5. **Performance**: Model latency and throughput properties
