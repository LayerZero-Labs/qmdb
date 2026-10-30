
## Terminology

- btree — B<sup>+</sup>tree
- BBN — bottom branch nodes
- BNID — branch node ID. Branch could be either BBN or UBN.
- LN — leaf nodes
- LNPN — a page number in the LN storage.
- UBN — upper branch nodes, everything that is not a LN or BBN.
- PN — a page number in a storage file.
- commit — an operation where a batch of changes applied to the database atomically.
- sync — an operation where one or more commits are flushed to the persistent storage.

## Preliminaries

This key-value database will be integrated into NOMT. It's specifically optimized for blockchain use cases. By leveraging unique workload features, we aim to outperform off-the-shelf solutions like mdbx or rocksdb.

Assumptions:

1. We can sacrifice durability, but consistency is required.
2. Recovery time is not a top priority.
3. pipelining: STF should not be blocked by `commit`.
4. lookup time should be consistent over time.
5. Memory consumption should be predictable. The btree branches should always be cached, and there should be a soft limit on the amount of cached data before flushing.
6. we assume that the btree index fits into memory (how many GiB is that for a billion of items?).
7. Keys are fixed length (32 or possibly 48 bytes) with high entropy. Values are typically small, around 32 bytes.
8. The database size generally grows over time. It doesn't shrink significantly.

Underlying media assumptions:

- an aligned write to a 512 byte sector is atomic, even when issued as part of a larger write request.
- data in sectors we don’t write is unaffected
- fsync or fdatasync means the data is durable
- we read back what is written

The implementation generally assumes ext4 (journal=ordered), io_uring, O_DIRECT.

## Architecture

The database is built around a non-conventional pruned-top b+ tree style index. It operates on the following types of nodes:

1. Leaf Nodes. Those nodes directly contain data.
2. Bottom-level Branch Nodes. Those are branch nodes that directly reference the leaf nodes.
3. Branch Nodes. This includes everything that doesn't fall into the above categories.

In pruned-top B+ tree the branch nodes are always resident in memory. All non-bottom-level branches are never saved to disk. Instead, during recovery time, the bottom-level branch nodes are used for reconstruction the above layers of branches. Being a B+ tree (as compared to a B tree) the leaves are stored separately from the branches.

The database consists of the following files:

1. Manifest
2. BBN storage
3. LN storage 

BBN and LN storages are files consisting of pages. The size of a file is thereby a multiply of a page size.

Only one kind of pages is used in BBN storage. The page format is the same as the branch node format and is described below.

LN storage has two kinds of pages: 

- freelist pages. Each page header contains single link to the previous item. The body of the page contains a list of page numbers that are ready to be reused.
- leaf data pages.

The manifest file is what ties those together and provides atomicity. It contains the following data (non-exhaustive):

1. next BBN sequence number
1. PN of the head of the freelist in the LN storage
2. current commit sequence number
3. the size of the LN storage and BBN storage.

### Atomic Commits

The database guarantees that in the event of a crash the user will find the database in the last successfully synced commits. It's not possible to observe the database partially updated.

The database relies on shadow paging to achieve atomicity. In a broad sense it is a technique for managing updates and allowing recovery in case of system failures. It works roughly like this:

- Creates a copy (shadow) of a page when modifications are needed
- Keeps the original page intact until sync is completed
- Updates references to point to the new page upon successful commit
- Allows easy rollback by discarding shadow pages if sync fails

The primary foe we are defending against with shadow paging are system crashes. This is an umbrella term but what it means for us is that we may issue some writes and only subset of those writes in undefined order will actually get persisted. One of the tools in our kit is `fsync` which guarantees that the data landed on disk.

By first writing and ensuring the shadow pages were persisted on disk and then atomically activing the new data we can achieve crash consistency. Note that some old data need to be deactivated as well.

Here's a sketch how it works applied to this database.

Let's assume there is a BBN & LN files with some data in them. 

Each BBN page has:

- a BBN sequence number, or bbn_seqn, which doesn't change throughout the lifetime of that BBN. In case a BBN is split, it is deleted and two new BBNs are created with different sequence numbers.
- commit sequence number. The ordinal number of the commit this page was written at.

During the sync to the persistent storage, the BBNs & LNs that were changed since the last sync are detected and written out to the disk.

The BBNs that were added are written to a new page with the newly allocated bbn_seqn. The BBNs that were modified written as a whole to a new page as well with the same bbn_seqn. Removed BBN nodes (this can happen as part of a split) are written into a new page as a special tombstone marker, which basically signals that a BBN with the given bbn_seqn is no longer valid.

BBNs, being the bottom-level branch nodes, reference leaves. Therefore, all the leaves that the new BBN pages reference must be either persisted to the LN storage file during the sync or be already present.

Finally, when the changes to those files are synced and ensured that they are persisted (via `fsync`), the manifest is updated atomically with the latest commit sequence number.

Let's examine what happens if a system crash happens just **after** the final manifest update. During startup the BBN storage file is read in whole. Final manifest update implies that all the BBNs & LNs pages written correctly and in full. Newly written BBNs reference the newest commit sequence number. It's possible that the file contains more than one BBN with the same bbn_seqn, this happens if that BBN was updated or removed. In that case, the BBN with highest bbn_seqn wins and the other is discarded as stale. Only the latest versions of BBNs are read making the commit consistent.

Now, let's look what happens if a system crash happens at any point after the start of sync but **before** the final manifest update. 

To reiterate the assumptions:

- we can expect that a random subset of writes could reach the disk.
- some page writes could be torn since we assume only sector writes are atomic.

First, let's reduce the problem by addressing the latter point. Say e.g. a page of 4 KiB consists of 8 sectors. Only the first sector, that contains the header of the page containing bbn_seqn, commit sequence number, matters. It doesn't matter what is stored in the body of the page if the page is ignored. Therefore, ostensibly, we can focus only on two cases: the header was updated and the header was not updated.

Next, due to shadow paging we do not overwrite any live data and thus we will only write into parts of the file which are unused (e.g. stale BBN nodes) or newly allocated space. We can rule out the latter case since we can easily detect writes into the newly allocated space (e.g. by saving the prior file size in the manifest). Building on top of the claim in the previous paragraph, we can conclude that either the header of the stale BBN is updated or it's not updated. In either case, the fact that that BBN is stale is unchanged and consequently will be ignored.

## Code Design

There is a `Tree` structure, 

Logically the `Tree` owns the following pieces of data (logically, non-exhausive):

- sequence number. Incremented by each `commit`.
- branch node pool
- staging map (btreemap or hashmap that store the key value pairs):
    - primary staging map
    - secondary staging map

and offers the following logical operations:

- `open() → Tree`, open or recover the database from the given wal instance.
- `commit([KV])`. Change the btree in the specified way. After this returns the get method below will return the new data.
- `lookup(K)`,
- `sync()`, dump the current changes to the underlying storage medium. Doesn't block commits. Blocks on sync. 
    > NOTE: in practice this could take different forms, e.g. it could be split into several methods (one for writing WAL, second for syncing, etc).

`commit` stores write are stored in the primary staging map. Commit does not alter the index.
`lookup` first checks the primary staging and if not found checks the secondary staging and if not found checks the index and hits the disk.

### Branch Node Pool

The branch node pool acts as a storage of nodes of the btree. It is responsible for storage, allocation and deallocation of the nodes.

Upon allocation of a node, the branch node pool returns BNID. BNID — is an interior node ID and it is distinct from those that live in the Tree and get stored in the index. Those BNIDs are just pointers to the in memory, think of those ids as similar those that are returned from Slab.

To fetch or to deallocate a node, the user passes the node's BNID. The nodes have a uniform format irregardless of their level.

A branch node is the following:

```rust=
bbn_seqn: u64          // The sequence number of this BBN.
                       // On disk, two nodes with the same seqn
                       // will be considered the same, and
                       // and the one with the latest valid 
                       // sync_seqn wins.

sync_seqn: u32       // the sequence number of the commit under
                       // which this node was created. 
                       // Important for BBNs only.

bbn_pn: u32            // the page number this is stored under, 
                       // if this is a BBN, 0 otherwise.

n: u16                 // item count
prefix_len: u8         // bits
separator_len: u8      // bits
prefix: var_bits
padding                // to next whole byte
separators: [var_bits] // n * separator_len
padding                // to next whole byte
node_pointers: [LNPN or BNID]   // n * 32
```

> NOTE: bbn_seqn, sync_seqn, bbn_pn are only relevant to the BBNs. However, because the branch node format is shared between the bottom-level branch nodes and upper-level branch nodes, 16 bytes of memory are wasted for every upper-level branch node. With the expected index size of 10 GiB, this wastage would amount to 40 MiB, which seems to be bearable. As future optimization, we could adjust the in-memory format of UBN in such a way, that those fields could be used for separators and node pointers, by e.g. borrowing a bit from bbn_seqn or reserving a niche value for sync_seqn.

### `sync` op

`sync` works as follows:

- assert that the secondary staging is empty.
- move the primary staging to secondary staging.
    (from this point on, the commits will be editing the primary staging.)
- a new version of the index is built from the secondary staging.
    - the untouched nodes from the previous index are reused as is via references.
- then atomically
    - the secondary staging is discarded.
    - the new index replaces the old one.
    - the nodes of the old index are freed up.
    - the new BBNs and LNs are dumped into io engine and other sync-stuff is performed like metadata fsync.


### BBN Dumper

BBN IO is write-only during normal operation and as such there is only a facility to dump BBNs on disk.

An essential component of the BBN IO is the freelist. It tracks the page numbers ready to be overwritten.

The updates to BBN storage applied in batches. A batch contains a list of nodes that should be written. Those could be updated nodes (bbn_seqn of which is already present in the file), new nodes (nodes with unique bbn_seqn) and tombstone nodes (a node that marks end-of-life of the given bbn_seqn). 

The general algorithm is as follows:

1. allocate PNs from the freelist for all the nodes written.
2. if we are N PNs short then increase the file space by calling ftruncate(M), where M is the next multiply of the bulk allocation size.
3. issue IO write requests for each node-PN pair to the BBN storage fd.
4. fsync on the BBN storage fd.
5. add all the deallocated BBNs into the freelist. The tombstones are not considered immediately deallocated. Instead, they should be deallocated in the next sync.

### Leaf Store

The `LeafStore` struct manages leaves. It's responsible for management (allocation and deallocation) and querying the LNs by their LNPN.

It maintains an in-memory copy of the freelist to facilitate the page management. The allocation is performed in LIFO order. The allocations are performed in batches to amortize the IO for the freelist and metadata updates (growing the file in case freelist is empty).

The leaf store doesn't perform caching. When querying the leaf store returns a handle to a page. As soon as the handle is dropped, the data becomes inaccessible and another disk roundtrip would be required to access the data again.

### btree handling logic

There are 3 essential btree handling operations:

1. update
1. lookup
1. reconstruct

#### Update

The btree update algorithm accepts:

1. a list of key value pairs,
2. a handle to the Branch Node Pool. 
3. the commit sequence number

The algorithm is expected to build a new tree that reflects changes after adding or removing the specified key pairs. The algorithm returns:

- the BNID of the root of the new tree.
- the list of BNIDs of the new BBNs.
- the list of BNIDs of nodes obsoleted by the new tree.
- the list of LNPNs of newly created leaves, the list of LNPNs of obsoleted leaves.

#### Reconstruct

Upon the startup, we need to materialize the tree in the Branch Node Pool. Only the bottom-level nodes are available at this point and they are stored on disk. The objective of `reconstruct` is:

1. to read the BBNs from the file, 
2. add the relevant nodes into the Branch Node Pool, 
3. recompute the branch nodes for the upper levels of the tree and
4. return the BNID of the root.

Apart from btree reconstruction, this function has the effect of initializing the BBN dumper, specifically, its freelist. All read tombstones should be scheduled for freeing after the next sync.

## Future Work

**Combine Tombstones**. There is no reason why we need to reserve an entire page for each tombstone. The fact that the nodes are written in batches allow us to combine all the tombstone markers into a single page.
