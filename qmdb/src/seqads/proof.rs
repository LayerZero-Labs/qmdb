use crate::indexer::Indexer;
use crate::seqads::{entry_updater::EntryUpdater, SeqAdsWrap};
use crate::utils::byte0_to_shard_id;
use crate::utils::hasher::Hash32;
use crate::{
    def::{
        calc_max_level, is_compactible, DEFAULT_ENTRY_SIZE, OP_CREATE, OP_DELETE, OP_WRITE,
        SHARD_COUNT, TWIG_SHIFT,
    },
    entryfile::{EntryBz, EntryVec},
    merkletree::Tree,
    stateless::node::{Node, ValuePair},
    stateless::utils::{level0nth_to_level8nth, sn_to_level0nth},
    stateless::witness::Witness,
    tasks::Task,
    utils::changeset::ChangeSet,
    AdsCore,
};
use core::panic;
use std::{collections::HashSet, vec};

impl<T: Task + 'static> SeqAdsWrap<T> {
    pub fn get_root_hash_list(&self) -> Vec<Hash32> {
        let mut root_hash_list = Vec::with_capacity(SHARD_COUNT);
        let entry_flusher = &self.ads.entry_flusher.lock().unwrap();
        for shard_id in 0..SHARD_COUNT {
            let shard = &*entry_flusher.shards[shard_id];
            let tree = &shard.tree;
            let max_level = calc_max_level(tree.youngest_twig_id) as u8;
            let root_hash = tree
                .get_hashes_by_pos_list(&vec![(max_level, 0)])
                .pop()
                .unwrap();
            root_hash_list.push(root_hash);
        }
        root_hash_list
    }

    pub fn get_extra_data_for_proof(&self) -> (Vec<u64>, Vec<u64>, Vec<u64>) {
        let mut sn_end_list = Vec::with_capacity(SHARD_COUNT);
        let mut sn_start_list = Vec::with_capacity(SHARD_COUNT);
        let mut active_count_list = Vec::with_capacity(SHARD_COUNT);

        let entry_flusher = &self.ads.entry_flusher.lock().unwrap();
        for shard_id in 0..SHARD_COUNT {
            let shard = &*entry_flusher.shards[shard_id];
            let updater = shard.updater.lock().unwrap();
            sn_end_list.push(updater.sn_end);
            sn_start_list.push(updater.sn_start);
            active_count_list.push(updater.indexer.len(shard_id) as u64);
        }
        (sn_end_list, sn_start_list, active_count_list)
    }

    pub fn get_proof(
        &self,
        height: i64,
        tx_input_key_hash_list: &Vec<Hash32>, // tx read has existed key hash list
        change_set: &ChangeSet,
        non_inclusion_key_hash_list_arr: &Vec<Vec<Hash32>>, // tx read not existed key hash list
    ) -> (
        Vec<Vec<u8>>, /* witness_bz_list */
        Vec<u8>,      /* entry_vec */
        Vec<Vec<u8>>, /* merkle_nodes_list */
    ) {
        let mut witness_bz_list = Vec::with_capacity(SHARD_COUNT);
        let mut entry_vec = EntryVec::new();
        let mut merkle_nodes_list = Vec::with_capacity(SHARD_COUNT);

        let entry_flusher = &self.ads.entry_flusher.lock().unwrap();

        let mut temp_buf = vec![0; DEFAULT_ENTRY_SIZE];

        for shard_id in 0..SHARD_COUNT {
            let updater = entry_flusher.shards[shard_id].updater.lock().unwrap();
            let indexer = updater.indexer.clone();

            let mut compact_start = updater.compact_start;
            let mut sn_start = updater.sn_start;
            let mut sn_end = updater.sn_end;
            let mut active_count = updater.indexer.len(updater.shard_id);
            let mut deactived_sn_list = vec![];

            let mut loaded_entry_key_hash_set = HashSet::new();

            change_set.run_in_shard(shard_id, |op, key_hash: &Hash32, _k, _v, _r| match op {
                OP_CREATE => {
                    if !self.read_pre_entry(
                        height,
                        &indexer,
                        shard_id,
                        _k,
                        key_hash,
                        &mut temp_buf,
                        -1,
                    ) {
                        panic!("OP_CREATE not found pre_entry : {:?}", _k);
                    }
                    let entry_bz = EntryBz { bz: &temp_buf };
                    deactived_sn_list.push(entry_bz.serial_number());
                    if !loaded_entry_key_hash_set.contains(&entry_bz.key_hash()) {
                        entry_vec.add_entry_bz(&entry_bz);
                        loaded_entry_key_hash_set.insert(entry_bz.key_hash());
                    }
                    sn_end += 2;
                    active_count += 1;
                    self.try_compact(
                        &updater,
                        active_count,
                        &deactived_sn_list,
                        &mut compact_start,
                        &mut sn_start,
                        &mut sn_end,
                        &mut entry_vec,
                        &mut temp_buf,
                        &mut loaded_entry_key_hash_set,
                    );
                    self.try_compact(
                        &updater,
                        active_count,
                        &deactived_sn_list,
                        &mut compact_start,
                        &mut sn_start,
                        &mut sn_end,
                        &mut entry_vec,
                        &mut temp_buf,
                        &mut loaded_entry_key_hash_set,
                    );
                }
                OP_WRITE => {
                    let pos = self.read_old_entry(
                        height,
                        &indexer,
                        shard_id,
                        _k,
                        key_hash,
                        &mut temp_buf,
                    );
                    if pos < 0 {
                        panic!("OP_WRITE not found old_entry : {:?}", _k);
                    }
                    let entry_bz = EntryBz { bz: &temp_buf };
                    deactived_sn_list.push(entry_bz.serial_number());
                    if !loaded_entry_key_hash_set.contains(&entry_bz.key_hash()) {
                        entry_vec.add_entry_bz(&entry_bz);
                        loaded_entry_key_hash_set.insert(entry_bz.key_hash());
                    }
                    sn_end += 1;
                    self.try_compact(
                        &updater,
                        active_count,
                        &deactived_sn_list,
                        &mut compact_start,
                        &mut sn_start,
                        &mut sn_end,
                        &mut entry_vec,
                        &mut temp_buf,
                        &mut loaded_entry_key_hash_set,
                    );
                }
                OP_DELETE => {
                    let old_entry_pos = self.read_old_entry(
                        height,
                        &indexer,
                        shard_id,
                        _k,
                        key_hash,
                        &mut temp_buf,
                    );
                    if old_entry_pos < 0 {
                        panic!("OP_DELETE not found old_entry : {:?}", _k);
                    }
                    let entry_bz = EntryBz { bz: &temp_buf };
                    deactived_sn_list.push(entry_bz.serial_number());
                    if !loaded_entry_key_hash_set.contains(&entry_bz.key_hash()) {
                        entry_vec.add_entry_bz(&entry_bz);
                        loaded_entry_key_hash_set.insert(entry_bz.key_hash());
                    }

                    if !self.read_pre_entry(
                        height,
                        &indexer,
                        shard_id,
                        _k,
                        key_hash,
                        &mut temp_buf,
                        old_entry_pos,
                    ) {
                        panic!("OP_DELETE not found pre_entry : {:?}", _k);
                    }
                    let entry_bz = EntryBz { bz: &temp_buf };
                    deactived_sn_list.push(entry_bz.serial_number());
                    if !loaded_entry_key_hash_set.contains(&entry_bz.key_hash()) {
                        entry_vec.add_entry_bz(&entry_bz);
                        loaded_entry_key_hash_set.insert(entry_bz.key_hash());
                    }
                    active_count -= 1;
                    sn_end += 1;
                }
                _ => panic!("unexpected op"),
            });

            // add existed key entry
            let cur_shard_hash_list = tx_input_key_hash_list
                .iter()
                .filter(|key_hash| byte0_to_shard_id(key_hash[0]) == shard_id)
                .collect::<Vec<&Hash32>>();
            for &key_hash in &cur_shard_hash_list {
                if loaded_entry_key_hash_set.contains(key_hash) {
                    continue;
                }

                let pos =
                    self.read_old_entry(height, &indexer, shard_id, &[], key_hash, &mut temp_buf);
                if pos < 0 {
                    panic!("get_proof:tx_input_key_hash {:?} not found", key_hash);
                }
                let entry_bz = EntryBz { bz: &temp_buf };
                entry_vec.add_entry_bz(&entry_bz);
                loaded_entry_key_hash_set.insert(entry_bz.key_hash());
            }

            // add not existed key entry
            let non_inclusion_key_hash_list = &non_inclusion_key_hash_list_arr[shard_id];
            for key_hash in non_inclusion_key_hash_list {
                if loaded_entry_key_hash_set.contains(key_hash) {
                    panic!("get_proof:tx_input_key_hash {:?} not found", key_hash);
                }

                if !self.read_pre_entry(
                    height,
                    &indexer,
                    shard_id,
                    &[],
                    key_hash,
                    &mut temp_buf,
                    -1,
                ) {
                    panic!("get_proof:non_inclusion_key_hash {:?} not found", key_hash);
                }
                let entry_bz = EntryBz { bz: &temp_buf };
                entry_vec.add_entry_bz(&entry_bz);
                loaded_entry_key_hash_set.insert(entry_bz.key_hash());
            }

            let mut touched_sn_list: Vec<_> = entry_vec
                .enumerate(shard_id)
                .map(|(_, e)| e.serial_number())
                .collect();
            //  new(change_and_compact) entry sn
            let old_sn_end = updater.sn_end;
            touched_sn_list.extend((0..(sn_end - old_sn_end)).map(|i| old_sn_end + i));

            // scan entry for compact
            let only_active_bits_sn_list = (updater.sn_start..sn_start).collect::<Vec<u64>>();
            let witness = get_witness_from_tree(
                shard_id,
                &touched_sn_list,
                &only_active_bits_sn_list,
                &entry_flusher.shards[shard_id].tree,
            );
            let (witness_bz, merkle_nodes) = witness.encode_witness(&entry_vec);
            witness_bz_list.push(witness_bz);
            merkle_nodes_list.push(merkle_nodes);
        }
        (witness_bz_list, entry_vec.to_bytes(), merkle_nodes_list)
    }

    fn try_compact(
        &self,
        updater: &EntryUpdater,
        active_count: usize,
        deactived_sn_list: &Vec<u64>,
        compact_start: &mut i64,
        sn_start: &mut u64,
        sn_end: &mut u64,
        entry_vec: &mut EntryVec,
        temp_buf: &mut Vec<u8>,
        loaded_entry_key_hash_set: &mut HashSet<Hash32>,
    ) {
        if !is_compactible(
            updater.utilization_div,
            updater.utilization_ratio,
            updater.compact_thres,
            active_count,
            *sn_start,
            *sn_end,
        ) {
            return;
        }

        loop {
            temp_buf.resize(DEFAULT_ENTRY_SIZE, 0);
            let size = updater
                .entry_file
                .read_entry(*compact_start, &mut temp_buf[..]);
            if temp_buf.len() < size {
                temp_buf.resize(size, 0);
                updater
                    .entry_file
                    .read_entry(updater.compact_start, &mut temp_buf[..]);
            }
            *compact_start += size as i64;

            let old_entry = EntryBz {
                bz: &temp_buf[..size],
            };
            if deactived_sn_list.contains(&old_entry.serial_number()) {
                continue;
            }

            if !updater
                .indexer
                .key_exists(&old_entry.key_hash(), *compact_start - size as i64, 0)
            {
                continue;
            }

            *sn_start = old_entry.serial_number() + 1;
            *sn_end += 1;
            if !loaded_entry_key_hash_set.contains(&old_entry.key_hash()) {
                entry_vec.add_entry_bz(&old_entry);
                loaded_entry_key_hash_set.insert(old_entry.key_hash());
            }
            return;
        }
    }

    fn read_old_entry(
        &self,
        height: i64,
        indexer: &Indexer,
        shard_id: usize,
        _k: &[u8],
        key_hash: &Hash32,
        buf: &mut Vec<u8>,
    ) -> i64 {
        let mut pos = -1;
        indexer.for_each_value(height, key_hash, |file_pos| -> bool {
            let size = self.read_entry_by_pos(_k, key_hash, shard_id, file_pos, buf);
            if size == 0 {
                return false;
            }
            let old_entry = EntryBz { bz: &buf[..size] };
            if &old_entry.key_hash() == key_hash {
                pos = file_pos;
                buf.truncate(size);
                return true;
            }
            false
        });
        pos
    }

    pub fn read_pre_entry(
        &self,
        height: i64,
        indexer: &Indexer,
        shard_id: usize,
        _k: &[u8],
        key_hash: &Hash32,
        buf: &mut Vec<u8>,
        self_pos: i64,
    ) -> bool {
        let mut found = false;
        indexer.for_each_adjacent_value(height, key_hash, |_k64_adj, file_pos| -> bool {
            if file_pos == self_pos {
                return false; // skip myself entry, continue loop
            }
            let size = self.read_entry_by_pos(_k, key_hash, shard_id, file_pos, buf);
            if size == 0 {
                return false;
            }
            let prev_entry = EntryBz { bz: &buf[..size] };
            if self_pos >= 0 && prev_entry.next_key_hash() == key_hash {
                found = true;
            } else if prev_entry.key_hash() < *key_hash
                && &key_hash[..] < prev_entry.next_key_hash()
            {
                found = true;
            }
            if found {
                buf.truncate(size);
                return true;
            }
            false
        });
        found
    }

    // read entry by pos, if buf is too small, read again
    fn read_entry_by_pos(
        &self,
        key: &[u8],
        key_hash: &Hash32,
        shard_id: usize,
        file_pos: i64,
        buf: &mut Vec<u8>,
    ) -> usize {
        buf.resize(DEFAULT_ENTRY_SIZE, 0);
        let (mut size, mut buf_too_small) =
            self._read_entry_by_pos(key, key_hash, shard_id, file_pos, buf);
        if buf_too_small {
            buf.resize(size, 0);
            (size, buf_too_small) = self._read_entry_by_pos(key, key_hash, shard_id, file_pos, buf);
            if buf_too_small {
                panic!("buf too small");
            }
        }
        size
    }

    fn _read_entry_by_pos(
        &self,
        key: &[u8],
        key_hash: &Hash32,
        shard_id: usize,
        file_pos: i64,
        buf: &mut [u8],
    ) -> (usize, bool) {
        let mut size = 0;
        let mut buf_too_small = false;

        self.ads.entry_cache.lookup(shard_id, file_pos, |entry_bz| {
            if AdsCore::check_entry(key_hash, key, &entry_bz) {
                size = entry_bz.len();
                if buf.len() < size {
                    buf_too_small = true;
                } else {
                    buf[..size].copy_from_slice(entry_bz.bz);
                }
            }
        });
        if size > 0 {
            return (size, buf_too_small); //stop loop if key matches or buf is too small
        }
        size = self.ads.entry_files[shard_id].read_entry(file_pos, buf);
        if buf.len() < size {
            return (size, true); //stop loop if buf is too small
        }
        (size, buf_too_small) // stop loop if key matches
    }
}

// get all the leaf nodes and internal nodes that must be included in
// the witness data to prove the accessed entries
// sn_list must be sorted in ascending order
fn get_witness_from_tree(
    shared_id: usize,
    sn_list: &Vec<u64>,
    only_active_bits_sn_list: &Vec<u64>,
    tree: &Tree,
) -> Witness {
    if sn_list.is_empty() {
        // if sn_list is empty, then compaction is not needed, which means
        // only_active_bits_sn_list is also empty
        let max_level = calc_max_level(tree.youngest_twig_id) as u8;
        let root_hash = tree
            .get_hashes_by_pos_list(&vec![(max_level, 0)])
            .pop()
            .unwrap();
        return Witness::new(
            shared_id,
            vec![Node {
                value: Some(Box::new(ValuePair {
                    old: root_hash,
                    new: root_hash,
                })),
                level: max_level,
                nth: 0,
            }],
        );
    }

    let max_sn = *sn_list.last().unwrap();
    let max_level = calc_max_level(max_sn >> TWIG_SHIFT) as u8;

    let mut pos_set = HashSet::<(u8, u64)>::new();
    for leaf in sn_list.iter().map(|&sn| sn_to_level0nth(sn)) {
        if leaf % 4096 >= 2048 {
            panic!("not at the left tree of a twig");
        }
        pos_set.insert((0, leaf));
        _get_pos_list(max_level - 1, &mut pos_set, 0, leaf);
        let activebits_leaf_nth = level0nth_to_level8nth(leaf);
        pos_set.insert((8, activebits_leaf_nth));
        _get_pos_list(max_level - 1, &mut pos_set, 8, activebits_leaf_nth);
    }

    for &sn in only_active_bits_sn_list.iter() {
        let leaf = sn_to_level0nth(sn);
        let activebits_leaf_nth = level0nth_to_level8nth(leaf);
        pos_set.insert((8, activebits_leaf_nth));
        _get_pos_list(max_level - 1, &mut pos_set, 8, activebits_leaf_nth);
    }

    //  clean up the nodes that are not necessary
    for leaf in sn_list.iter().map(|&sn| sn_to_level0nth(sn)) {
        for level in 1..=max_level {
            pos_set.remove(&(level, leaf / u64::pow(2, level as u32)));
        }
        let mut activebits_leaf_nth = level0nth_to_level8nth(leaf);
        for level in 9..=max_level {
            activebits_leaf_nth /= 2;
            pos_set.remove(&(level, activebits_leaf_nth));
        }
    }

    let pos_list = _sort_pos_list(&pos_set);
    let mut nodes = Vec::<Node>::with_capacity(pos_list.len());
    for (i, hash) in tree.hash_iter(&pos_list) {
        let pos = pos_list[i];
        let mut p = Node::new(pos.0, pos.1, true);
        p.set_old(&hash);
        nodes.push(p);
    }
    Witness::new(shared_id, nodes)
}

// add siblings and ancestors from the leaf to the root
fn _get_pos_list(
    max_level: u8,
    included_nodes: &mut HashSet<(u8, u64)>,
    leaf_level: u8,
    leaf_nth: u64,
) {
    included_nodes.insert((leaf_level, leaf_nth));
    let mut nth = leaf_nth;
    for level in leaf_level..=max_level {
        let sib_nth = nth ^ 1;
        included_nodes.insert((level, sib_nth));
        nth /= 2;
    }
}

fn _sort_pos_list(nodes_set: &HashSet<(u8, u64)>) -> Vec<(u8, u64)> {
    let mut leaf_and_pos_list = nodes_set.iter().fold(
        Vec::<(u64, (u8, u64))>::with_capacity(nodes_set.len()),
        |mut acc, &pos| {
            let leaf = pos.1 * u64::pow(2, pos.0 as u32);
            acc.push((leaf, pos));
            acc
        },
    );

    leaf_and_pos_list.sort_by_key(|&(key, _)| key);
    leaf_and_pos_list
        .into_iter()
        .map(|(_, value)| value)
        .collect()
}

#[cfg(test)]
mod tests {
    use serial_test::serial;

    use crate::{
        def::TWIG_MASK,
        merkletree::{check, helpers::build_test_tree, Tree},
        test_helper::TempDir,
    };

    use super::*;

    #[test]
    #[serial]
    fn test_verify_witness() {
        let dir_name = "./witness";
        let _tmp_dir = TempDir::new(dir_name);
        let (tree, _, _, _) = create_tree(dir_name, true);
        // tree.print();

        let sns = vec![0, 9787];
        let witness = get_witness_from_tree(0, &sns, &vec![], &tree);
        #[cfg(not(feature = "tee_cipher"))]
        let b = Witness::verify_witness(
            &witness,
            &[],
            &[
                27, 123, 1, 38, 223, 79, 224, 97, 235, 65, 238, 146, 111, 171, 122, 46, 58, 230,
                106, 131, 68, 123, 20, 137, 82, 71, 199, 198, 125, 31, 181, 10,
            ],
            &[
                241, 14, 44, 193, 141, 198, 155, 170, 45, 152, 247, 141, 70, 241, 149, 222, 117,
                111, 153, 149, 212, 25, 207, 82, 212, 20, 117, 216, 193, 236, 219, 73,
            ],
        );
        #[cfg(feature = "tee_cipher")]
        let b = Witness::verify_witness(
            &witness,
            &[],
            &[
                27, 123, 1, 38, 223, 79, 224, 97, 235, 65, 238, 146, 111, 171, 122, 46, 58, 230,
                106, 131, 68, 123, 20, 137, 82, 71, 199, 198, 125, 31, 181, 10,
            ],
            &[
                241, 14, 44, 193, 141, 198, 155, 170, 45, 152, 247, 141, 70, 241, 149, 222, 117,
                111, 153, 149, 212, 25, 207, 82, 212, 20, 117, 216, 193, 236, 219, 73,
            ],
        );
        assert!(b);
    }

    fn create_tree(dir_name: &str, sync: bool) -> (Tree, Vec<i64>, u64, Vec<Vec<u8>>) {
        let deact_sn_list: Vec<u64> = (0..2048)
            .chain(vec![5000, 5500, 5700, 5813, 6001])
            .collect();
        let (mut tree, pos_list, serial_number, entry_bzs) =
            build_test_tree(dir_name, &deact_sn_list, TWIG_MASK as i32 * 4, 1600);

        let pos_list = pos_list.into_iter().map(|r| r.unwrap()).collect();

        if sync {
            let n_list = tree.flush_files(0, 0);
            let n_list = tree.upper_tree.evict_twigs(n_list, 0, 0);
            tree.upper_tree
                .sync_upper_nodes(n_list, tree.youngest_twig_id);
            check::check_hash_consistency(&tree);
        }
        (tree, pos_list, serial_number, entry_bzs)
    }
}

// ... rest of the existing code ...
