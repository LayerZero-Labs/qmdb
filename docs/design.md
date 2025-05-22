# QMDB's General Ideas

## 1.0 In the whole history of a blockchain, how can we prove which KV pairs were created/updated at which blocks?

When a Key-Value pair is created/updated at a block of Height, we create such an Entry:

```text
Entry := (Height, Key, Value, SerialNum)
```

Through the history of the blockchain, we get a sequence of such entries. The SerialNum field shows an entry's postion in this sequence.

Then we build a balanced binary Merkle tree, with hash values of these entries as leaves. If the count of entries is not $2^N$, we just add null entries for padding, as is shown below \(the null entries are grey\).

<img src="figures/ADS_1.png" width="500">

Using the binary tree, we can prove the existence of an individual entry.

Of cause, in the implementation, we do not really add null entries for padding. Instead, we only need to add a null node at each level of the binary tree, as is shown below.

<img src="figures/ADS_2.png" width="500">

## 2.0 How to store such a Merkle tree

If we keep the whole Merkle tree in DRAM, then we can provide the proofs very fast. But unfortunately, it will cost a huge amount of DRAM so it is not feasible. A reasonable trade-off is: since the upper levels is more likely to get accessed than the lower levels, we can keep the upper levels in DRAM and the lower levels on SSD.

In the current implementation of QMDB, we store the lowest 11 levels of nodes \(which correspond to 2048 entries\) on SSD, as is shown below. Such a small sub-tree with 11 levels of nodes are named as a "twig". The youngest twig \(in the read circle\) has not got all its internal nodes finalized, since the 2048 entries are not decided yet. So we buffer this youngest twig in DRAM. All the other twigs are stored on SSD.

Such a Merkle tree has two parts: in-DRAM upper levels of hash nodes and in-SSD lower levels of twigs. So we name it as "Twig Merkle Tree".

<img src="figures/ADS_3.png" width="500">

## 3.0 How to prove a KV-pair is up-to-date? In other words, since some height H, an KV-pair is left untouched \(not changed or deleted\)

An ActiveBit is attached to each entry. If ActiveBit==1, then this KV-pair is up-to-date; if ActiveBit==0, then this KV-pair has been changed or deleted after the height recorded in the entry.

These ActiveBits need to be random-accessed, because an ActiveBits can be cleared to zero at any time. So we must keep all the ActiveBits in DRAM using a bit array. This bit array is indexed using an entry's SerialNum.

## 4.0 Should we build a dedicated Merkle tree for these ActiveBits?

Although we do want to provide Merkle proofs for individual ActiveBits, a dedicated Merkle tree for ActiveBits is not necessary. A more memory-efficient implementation is to integrate the ActiveBits into the twigs. That is, a twig contains 2048 entries and the corresponded 2048 ActiveBits.

The small Merkle tree inside a twig now is made of two parts, as is shown below. The left part is a 11-level tree with 2048 entries as its leaves. And the right part is a three-level tree with 8 leaves, where each leave contains 256 ActiveBits. And the Merkle root of the entire twig, i.e. TwigRoot as is shown below, is a hash value calculated from roots of the left part and the right part.

<img src="figures/ADS_4.png" width="600">

## 5.0 How to prune the ActiveBits and entries?

We want to keep only the recent entries whose ActiveBits equal 1, and prune the old ones to save memory. But currently we can not make sure that the old enough entries are inactive \(i.e. having their ActiveBits==0\). Actually, even the oldest entries may still be active.

We have a counter measure: use redundant updates to compact the range of active entries. We fetch the oldest entries and overwrite their value using exactly the same values. Thus new entries are created and the oldest entries are deactived.

The following picture shows a "redundant update" operation. Two oldest active entries \(in the two red rectangles on the left side\) are fetched out and two new entries are generated \(in the red rectangle on the right side\). Then the ActiveBits of the oldest entries are cleared to 0 and the ActiveBits and the newly-created entries are set to 1.

Using this method, we can ensure the old enough entries are deactived if their SerialNums are less than a particular value. The twigs whose entries' SerialNums are all less than this particular value are refered to as "inactive twigs", or else "active twigs". an inactive twig has all its ActiveBits==0. Please note an active twig can also has all its ActiveBits==0. We say a twig is "active" only because it MAY have ActiveBits==1.

<img src="figures/ADS_5.png" width="600">

## 6.0 What are the possible states of twigs

We can describe a twig's status using several terms.

<img src="figures/ADS_6.png" width="600">

First term: "Youngest". There is only one youngest twig \(the green one in the above figure\). A youngest twig has null entries (i.e., non-finailzed ones). When all the 2048 entries are ready, it is no longer a youngest twig. Another twig with 2048 null entries will be created as the new youngest twig. Youngest twig is the only twig whose left 11-level Merkle tree is stored in DRAM. For the other twigs, the left 11-level Merkle trees are stored in SSD.

Second term: "Active". The oldest active twig is the oldest twig that contains at least one active entry. All the twigs which are younger than the oldest active twig are active twigs. A youngest twig is certainly active. In the above figure, the blue and green triangles are active twigs. The active twigs use DRAM to store their right three-level Merkle trees whose leaves are the ActiveBits, such that these bits can be cleared at any time.

Third term: "Pruned". The inactive twigs can be kept in SSD such that we can query about the historical KV-pairs of the blockchain. They can also be pruned if we not interested in these historical old KV-pairs. A non-pruned inactive twig is kept in SSD, and a pruned inactive twig is deleted from the SSD. In the above figure, the black triangles are non-pruned twigs and the orange triangles are pruned twigs.

A twig can be in one of four possible states:

1. Active and youngest
2. Active and non-youngest
3. Inactive and not-pruned. This states is also known as "evicted from DRAM", or "evicted" for short.
4. Inactive and pruned

## 7.0 How to provide exclusion proof? That is, we want to prove there are no other key's hash residing between hash A and hash B

We need to extend entries with a NextKeyHash field:

```text
Entry := (Height, Key, NextKeyHash, Value, SerialNum)
```

After Height, if some events happened between Key and NextKeyHash \(deletion of NextKey or insertion of a new key\), then this entry is sure to be deactived and a new entry will be created.

## 8.0 How to proof inclusion/exclusion at any height?

Now, using the information in entries, we can prove inclusion/exclusion at individual heights (H0, H1, .. Hn) at which the entries are written. But we can not prove at a middle height. For example, if no entry was written at H ∈ (H0, H1), how can prove inclusion/exclusion here?

So we need to extend entries with new fields "DeactivedSerialNum0" and "DeactivedSerialNum1" (the latter is optional):

```text
Entry := (Height, LastHeight, Key, NextKeyHash, Value, SerialNum, DeactivedSerialNum0, [DeactivedSerialNum1])
```

Every time a new entry is created and actived, one or two entries will be deactived:

1. If an KV pair gets new value, the entry of its old version is deactived. The new entry use DeactivedSerialNum0 to record the deactived old entry.
2. If an KV pair is created, new entries must be appended: one is for this created pair and the other is an entry whose NextKeyHash is changed. Each appended entry use DeactivedSerialNum0 to record its corresponding deactived entry.
3. If an KV pair is deleted, one entry will have its NextKeyHash changed. This entry uses DeactivedSerialNum0 and DeactivedSerialNum1 to record the deleted entry and its own corresponding old entry.

Now we can prove an entry is created at height Ha and deactived at height Hb. So in the range (Ha, Hb], we can prove inclusion of its Key and Value and the exclusion of any keys whose hash fall in the range (hash(Key), NextKeyHash).

## 9.0  How to query a KV pair fast?

We can build an ordered map, which maps a `(hash(key), height)` tuple to a 64-bit integer, which is the offset of the entry in file.

This index must support iterator, so hash maps do not work. We must use some tree structure, such as red-black tree \(in DRAM\), B/B+ tree \(in DRAM or in disk\), LSM tree \(in disk\). To balance speed and memory consumption, we use B-tree in DRAM to index the latest keys.

## 10.0 How to prune the inactive twigs from SSD?

A block at height H can create one or more entries. The ID of the oldest active twig seen by it is denoted as OldestActiveTwig\(H\). When we do not need to query the information about the blocks older than H, we can prune the twigs that are older than OldestActiveTwig\(H\).

Filesystem does not support truncate a file from the beginning to a middle point. So we must use a series of fixed-size small files to simulate one large file. Pruning from the beginning is deleting the first several small files. We name such a virtual large file as "head-prunable" file.

The content of twigs are seperated and store in two head-prunable files: one for the leaves, i.ie. the entries, and the other for the left 11-level Merkle tree. What about the right three-level Merkle tree? Well, for the inactive twigs, all the ActiveBits are zero, so the right three-level Merkle trees all have the same value.

The left 11-level Merkle trees occupies fixed-length bytes on disk. So it is easy to calculate any node's offset in file.
