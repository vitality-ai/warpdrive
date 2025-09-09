---- MODULE WarpDrive_SingleNode ----
\* TLA+ specification for WarpDrive single-node storage system
\* Simplified version without operation counting

EXTENDS Naturals, Sequences, FiniteSets, TLC

CONSTANTS 
    Users,           \* Set of possible users
    Keys,            \* Set of possible keys  
    Data             \* Set of possible data values

VARIABLES
    storage,         \* storage[user][key] = data (current stored data)
    metadata         \* metadata[user][key] = offset_size_list (database metadata)

TypeOK == 
    /\ storage \in [Users -> [Keys -> Data \cup {""}]]
    /\ metadata \in [Users -> [Keys -> Seq(Nat)]]

\* Initial state: empty storage and metadata
Init == 
    /\ storage = [u \in Users |-> [k \in Keys |-> ""]]
    /\ metadata = [u \in Users |-> [k \in Keys |-> <<>>]]

\* Helper: Check if key exists for user
KeyExists(user, key) == storage[user][key] # ""

\* PUT operation: Store data for a user-key pair
Put(user, key, data) ==
    /\ user \in Users
    /\ key \in Keys  
    /\ data \in Data
    /\ ~KeyExists(user, key)  \* Key must not exist
    /\ storage' = [storage EXCEPT ![user][key] = data]
    /\ metadata' = [metadata EXCEPT ![user][key] = <<1, Len(data)>>]

\* GET operation: Retrieve data for a user-key pair
Get(user, key) ==
    /\ user \in Users
    /\ key \in Keys
    /\ KeyExists(user, key)  \* Key must exist
    /\ UNCHANGED <<storage, metadata>>

\* DELETE operation: Remove data for a user-key pair
Delete(user, key) ==
    /\ user \in Users
    /\ key \in Keys
    /\ KeyExists(user, key)  \* Key must exist
    /\ storage' = [storage EXCEPT ![user][key] = ""]
    /\ metadata' = [metadata EXCEPT ![user][key] = <<>>]

\* UPDATE operation: Replace data for existing key
Update(user, key, newData) ==
    /\ user \in Users
    /\ key \in Keys
    /\ newData \in Data
    /\ KeyExists(user, key)  \* Key must exist
    /\ storage' = [storage EXCEPT ![user][key] = newData]
    /\ metadata' = [metadata EXCEPT ![user][key] = <<1, Len(newData)>>]

\* Next state relation - All operations enabled
Next == 
    \/ \E user \in Users, key \in Keys, data \in Data : Put(user, key, data)
    \/ \E user \in Users, key \in Keys : Get(user, key)
    \/ \E user \in Users, key \in Keys : Delete(user, key)
    \/ \E user \in Users, key \in Keys, data \in Data : Update(user, key, data)

\* Safety invariants
DataConsistency == 
    \A user \in Users, key \in Keys :
        KeyExists(user, key) <=> (Len(metadata[user][key]) > 0)

\* Consistency properties
Consistent == DataConsistency

\* Specification
Spec == Init /\ [][Next]_<<storage, metadata>>

\* Properties to check
THEOREM Properties == 
    Spec => []DataConsistency

====