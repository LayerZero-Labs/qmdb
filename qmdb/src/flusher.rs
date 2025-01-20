use crate::def::{
    LEAF_COUNT_IN_TWIG, MAX_PROOF_REQ, MIN_PRUNE_COUNT, PRUNE_EVERY_NBLOCKS, SHARD_COUNT,
    TWIG_SHIFT,
};
use crate::entryfile::{EntryBufferReader, EntryFile};
use crate::merkletree::proof::ProofPath;
use crate::merkletree::Tree;
#[cfg(feature = "slow_hashing")]
use crate::merkletree::UpperTree;
use crate::metadb::{MetaDB, MetaInfo};
use parking_lot::RwLock;
use std::sync::mpsc::{sync_channel, Receiver, SyncSender};
use std::sync::{Arc, Barrier, Condvar, Mutex};
use std::thread;

type RocksMetaDB = MetaDB;

pub struct BarrierSet {
    pub flush_bar: Barrier,
    pub metadb_bar: Barrier,
}

impl BarrierSet {
    pub fn new(n: usize) -> Self {
        Self {
            flush_bar: Barrier::new(n),
            metadb_bar: Barrier::new(n),
        }
    }
}

pub type ProofReqElem = Arc<(Mutex<(u64, Option<Result<ProofPath, String>>)>, Condvar)>;

pub struct Flusher {
    shards: Vec<Box<FlusherShard>>,
    meta: Arc<RwLock<RocksMetaDB>>,
    curr_height: i64,
    max_kept_height: i64,
    end_block_chan: SyncSender<Arc<MetaInfo>>,
}

impl Flusher {
    pub fn new(
        shards: Vec<Box<FlusherShard>>,
        meta: Arc<RwLock<RocksMetaDB>>,
        curr_height: i64,
        max_kept_height: i64,
        end_block_chan: SyncSender<Arc<MetaInfo>>,
    ) -> Self {
        Self {
            shards,
            meta,
            curr_height,
            max_kept_height,
            end_block_chan,
        }
    }

    pub fn get_proof_req_senders(&self) -> Vec<SyncSender<ProofReqElem>> {
        let mut v = Vec::with_capacity(SHARD_COUNT);
        for i in 0..SHARD_COUNT {
            v.push(self.shards[i].proof_req_sender.clone());
        }
        v
    }

    pub fn flush(&mut self, shard_count: usize) {
        loop {
            self.curr_height += 1;
            let prune_to_height = self.curr_height - self.max_kept_height;
            let bar_set = Arc::new(BarrierSet::new(shard_count));
            thread::scope(|s| {
                // let curr_height = self.curr_height;
                for shard in self.shards.iter_mut() {
                    let bar_set = bar_set.clone();
                    let curr_height = self.curr_height;
                    let meta = self.meta.clone();
                    let end_block_chan = self.end_block_chan.clone();
                    s.spawn(move || {
                        shard.flush(prune_to_height, curr_height, meta, bar_set, end_block_chan);
                    });
                }
            });
        }
    }

    pub fn get_entry_file(&self, shard_id: usize) -> Arc<EntryFile> {
        self.shards[shard_id].tree.entry_file_wr.entry_file.clone()
    }

    pub fn set_entry_buf_reader(&mut self, shard_id: usize, ebr: EntryBufferReader) {
        self.shards[shard_id].buf_read = Some(ebr);
    }
}

pub struct FlusherShard {
    buf_read: Option<EntryBufferReader>,
    tree: Tree,
    last_compact_done_sn: u64,
    shard_id: usize,
    proof_req_sender: SyncSender<ProofReqElem>,
    proof_req_receiver: Receiver<ProofReqElem>,
    #[cfg(feature = "slow_hashing")]
    upper_tree_sender: SyncSender<UpperTree>,
    #[cfg(feature = "slow_hashing")]
    upper_tree_receiver: Receiver<UpperTree>,
}

impl FlusherShard {
    pub fn new(tree: Tree, oldest_active_sn: u64, shard_id: usize) -> Self {
        #[cfg(feature = "slow_hashing")]
        let (ut_sender, ut_receiver) = sync_channel(2);
        let (pr_sender, pr_receiver) = sync_channel(MAX_PROOF_REQ);

        Self {
            buf_read: None,
            tree,
            last_compact_done_sn: oldest_active_sn,
            shard_id,
            proof_req_sender: pr_sender,
            proof_req_receiver: pr_receiver,
            #[cfg(feature = "slow_hashing")]
            upper_tree_sender: ut_sender,
            #[cfg(feature = "slow_hashing")]
            upper_tree_receiver: ut_receiver,
        }
    }

    pub fn handle_proof_req(&self) {
        loop {
            let pair = self.proof_req_receiver.try_recv();
            if pair.is_err() {
                break;
            }
            let pair = pair.unwrap();
            let (lock, cvar) = &*pair;
            let mut sn_proof = lock.lock().unwrap();
            let proof = self.tree.get_proof(sn_proof.0);
            sn_proof.1 = Some(proof);
            cvar.notify_one();
        }
    }

    pub fn flush(
        &mut self,
        prune_to_height: i64,
        curr_height: i64,
        meta: Arc<RwLock<RocksMetaDB>>,
        bar_set: Arc<BarrierSet>,
        end_block_chan: SyncSender<Arc<MetaInfo>>,
    ) {
        let buf_read = self.buf_read.as_mut().unwrap();
        loop {
            let mut file_pos: i64 = 0;
            let (is_end_of_block, expected_file_pos) = buf_read.read_next_entry(|entry_bz| {
                file_pos = self.tree.append_entry(&entry_bz).unwrap();
                for (_, dsn) in entry_bz.dsn_iter() {
                    self.tree.deactive_entry(dsn);
                }
            });
            if !is_end_of_block && file_pos != expected_file_pos {
                panic!("File_pos mismatch!");
            }
            if is_end_of_block {
                break;
            }
        }
        let (compact_done_pos, compact_done_sn, sn_end) = buf_read.read_extra_info();

        #[cfg(feature = "slow_hashing")]
        {
            if self.tree.upper_tree.is_empty() {
                let mut upper_tree = self.upper_tree_receiver.recv().unwrap();
                std::mem::swap(&mut self.tree.upper_tree, &mut upper_tree);
            }
            let mut start_twig_id: u64 = 0;
            let mut end_twig_id: u64 = 0;
            let mut ef_size: i64 = 0;
            if prune_to_height > 0 && prune_to_height % PRUNE_EVERY_NBLOCKS == 0 {
                let meta = meta.read_arc();
                (start_twig_id, _) = meta.get_last_pruned_twig(self.shard_id);
                (end_twig_id, ef_size) =
                    meta.get_first_twig_at_height(self.shard_id, prune_to_height);
                if end_twig_id == u64::MAX {
                    panic!(
                        "FirstTwigAtHeight Not Found shard={} prune_to_height={}",
                        self.shard_id, prune_to_height
                    );
                }
                let mut last_evicted_twig_id = compact_done_sn / (LEAF_COUNT_IN_TWIG as u64);
                last_evicted_twig_id = last_evicted_twig_id.saturating_sub(1);
                if end_twig_id > last_evicted_twig_id {
                    end_twig_id = last_evicted_twig_id;
                }
                if start_twig_id <= end_twig_id && end_twig_id < start_twig_id + MIN_PRUNE_COUNT {
                    end_twig_id = start_twig_id;
                } else {
                    self.tree.prune_twigs(start_twig_id, end_twig_id, ef_size);
                }
            }
            let del_start = self.last_compact_done_sn / (LEAF_COUNT_IN_TWIG as u64);
            let del_end = compact_done_sn / (LEAF_COUNT_IN_TWIG as u64);
            let tmp_list = self.tree.flush_files(del_start, del_end);
            let (entry_file_size, twig_file_size) = self.tree.get_file_sizes();
            let last_compact_done_sn = self.last_compact_done_sn;
            self.last_compact_done_sn = compact_done_sn;
            bar_set.flush_bar.wait();

            let youngest_twig_id = self.tree.youngest_twig_id;
            let shard_id = self.shard_id;
            let mut upper_tree = UpperTree::empty();
            std::mem::swap(&mut self.tree.upper_tree, &mut upper_tree);
            let upper_tree_sender = self.upper_tree_sender.clone();
            thread::spawn(move || {
                let n_list = upper_tree.evict_twigs(
                    tmp_list,
                    last_compact_done_sn >> TWIG_SHIFT,
                    compact_done_sn >> TWIG_SHIFT,
                );
                let (_new_n_list, root_hash) =
                    upper_tree.sync_upper_nodes(n_list, youngest_twig_id);
                let mut edge_nodes_bytes = Vec::<u8>::with_capacity(0);
                if prune_to_height > 0
                    && prune_to_height % PRUNE_EVERY_NBLOCKS == 0
                    && start_twig_id < end_twig_id
                {
                    edge_nodes_bytes =
                        upper_tree.prune_nodes(start_twig_id, end_twig_id, youngest_twig_id);
                }

                //shard#0 must wait other shards to finish
                if shard_id == 0 {
                    bar_set.metadb_bar.wait();
                }

                let mut meta = meta.write_arc();
                if !edge_nodes_bytes.is_empty() {
                    meta.set_edge_nodes(shard_id, &edge_nodes_bytes[..]);
                    meta.set_last_pruned_twig(shard_id, end_twig_id, ef_size);
                }
                meta.set_root_hash(shard_id, root_hash);
                meta.set_oldest_active_sn(shard_id, compact_done_sn);
                meta.set_oldest_active_file_pos(shard_id, compact_done_pos);
                meta.set_next_serial_num(shard_id, sn_end);
                if curr_height % PRUNE_EVERY_NBLOCKS == 0 {
                    meta.set_first_twig_at_height(
                        shard_id,
                        curr_height,
                        compact_done_sn / (LEAF_COUNT_IN_TWIG as u64),
                        compact_done_pos,
                    )
                }
                meta.set_entry_file_size(shard_id, entry_file_size);
                meta.set_twig_file_size(shard_id, twig_file_size);

                if shard_id == 0 {
                    meta.set_curr_height(curr_height);
                    let meta_info = meta.commit();
                    drop(meta);
                    match end_block_chan.send(meta_info) {
                        Ok(_) => {
                            //println!("{} end block", curr_height);
                        }
                        Err(_) => {
                            println!("end block sender exit!");
                            return;
                        }
                    }
                } else {
                    drop(meta);
                    bar_set.metadb_bar.wait();
                }
                upper_tree_sender.send(upper_tree).unwrap();
            });
        }

        #[cfg(not(feature = "slow_hashing"))]
        {
            let mut start_twig_id: u64 = 0;
            let mut end_twig_id: u64 = 0;
            let mut ef_size: i64 = 0;
            if prune_to_height > 0 && prune_to_height % PRUNE_EVERY_NBLOCKS == 0 {
                let meta = meta.read_arc();
                (start_twig_id, _) = meta.get_last_pruned_twig(self.shard_id);
                (end_twig_id, ef_size) =
                    meta.get_first_twig_at_height(self.shard_id, prune_to_height);
                if end_twig_id == u64::MAX {
                    panic!(
                        "FirstTwigAtHeight Not Found shard={} prune_to_height={}",
                        self.shard_id, prune_to_height
                    );
                }
                let mut last_evicted_twig_id = compact_done_sn / (LEAF_COUNT_IN_TWIG as u64);
                last_evicted_twig_id = last_evicted_twig_id.saturating_sub(1);
                if end_twig_id > last_evicted_twig_id {
                    end_twig_id = last_evicted_twig_id;
                }
                if start_twig_id <= end_twig_id && end_twig_id < start_twig_id + MIN_PRUNE_COUNT {
                    end_twig_id = start_twig_id;
                } else {
                    self.tree.prune_twigs(start_twig_id, end_twig_id, ef_size);
                }
            }
            let del_start = self.last_compact_done_sn / (LEAF_COUNT_IN_TWIG as u64);
            let del_end = compact_done_sn / (LEAF_COUNT_IN_TWIG as u64);
            let tmp_list = self.tree.flush_files(del_start, del_end);
            let (entry_file_size, twig_file_size) = self.tree.get_file_sizes();
            let last_compact_done_sn = self.last_compact_done_sn;
            self.last_compact_done_sn = compact_done_sn;
            bar_set.flush_bar.wait();

            let youngest_twig_id = self.tree.youngest_twig_id;
            let shard_id = self.shard_id;
            let upper_tree = &mut self.tree.upper_tree;
            let n_list = upper_tree.evict_twigs(
                tmp_list,
                last_compact_done_sn >> TWIG_SHIFT,
                compact_done_sn >> TWIG_SHIFT,
            );
            let (_new_n_list, root_hash) = upper_tree.sync_upper_nodes(n_list, youngest_twig_id);
            let mut edge_nodes_bytes = Vec::<u8>::with_capacity(0);
            if prune_to_height > 0
                && prune_to_height % PRUNE_EVERY_NBLOCKS == 0
                && start_twig_id < end_twig_id
            {
                edge_nodes_bytes =
                    upper_tree.prune_nodes(start_twig_id, end_twig_id, youngest_twig_id);
            }

            self.handle_proof_req();

            //shard#0 must wait other shards to finish
            if shard_id == 0 {
                bar_set.metadb_bar.wait();
            }

            let mut meta = meta.write_arc();
            if !edge_nodes_bytes.is_empty() {
                meta.set_edge_nodes(shard_id, &edge_nodes_bytes[..]);
                meta.set_last_pruned_twig(shard_id, end_twig_id, ef_size);
            }
            meta.set_root_hash(shard_id, root_hash);
            meta.set_oldest_active_sn(shard_id, compact_done_sn);
            meta.set_oldest_active_file_pos(shard_id, compact_done_pos);
            meta.set_next_serial_num(shard_id, sn_end);
            if curr_height % PRUNE_EVERY_NBLOCKS == 0 {
                meta.set_first_twig_at_height(
                    shard_id,
                    curr_height,
                    compact_done_sn / (LEAF_COUNT_IN_TWIG as u64),
                    compact_done_pos,
                )
            }
            meta.set_entry_file_size(shard_id, entry_file_size);
            meta.set_twig_file_size(shard_id, twig_file_size);

            if shard_id == 0 {
                meta.set_curr_height(curr_height);
                let meta_info = meta.commit();
                drop(meta);
                match end_block_chan.send(meta_info) {
                    Ok(_) => {
                        //println!("{} end block", curr_height);
                    }
                    Err(_) => {
                        println!("end block sender exit!");
                    }
                }
            } else {
                drop(meta);
                bar_set.metadb_bar.wait();
            }
        }
    }
}

#[cfg(test)]
mod flusher_tests {
    use crate::def::{
        DEFAULT_ENTRY_SIZE, DEFAULT_FILE_SIZE, ENTRIES_PATH, SENTRY_COUNT, SMALL_BUFFER_SIZE,
        TWIG_PATH, TWIG_SHIFT,
    };
    use crate::entryfile::helpers::create_cipher;
    use crate::entryfile::{
        entry::{entry_to_bytes, sentry_entry, Entry},
        entrybuffer,
    };
    use crate::flusher::{Flusher, FlusherShard};
    use crate::merkletree::check::check_hash_consistency;
    use crate::merkletree::{
        proof::check_proof,
        recover::{bytes_to_edge_nodes, recover_tree},
        Tree,
    };
    use crate::metadb::MetaDB;
    use crate::test_helper::TempDir;
    use parking_lot::RwLock;
    use std::sync::mpsc::sync_channel;
    use std::sync::Arc;
    use std::thread::sleep;
    use std::{fs, thread, time};

    #[test]
    fn test_flusher() {
        let _dir = TempDir::new("./test_flusher");
        let data_dir = "./test_flusher/data";
        let dir_entry = format!("{}/{}{}", data_dir, ENTRIES_PATH, ".test");
        let _ = fs::create_dir_all(dir_entry);
        let dir_twig = format!("{}/{}{}", data_dir, TWIG_PATH, ".test");
        let _ = fs::create_dir_all(dir_twig);
        let cipher = create_cipher();

        let (eb_sender, _eb_receiver) = sync_channel(2);
        let meta = Arc::new(RwLock::new(MetaDB::with_dir("./test_flusher/metadb", None)));

        let mut flusher = Flusher {
            shards: Vec::with_capacity(1),
            meta,
            curr_height: 0,
            max_kept_height: 1000,
            end_block_chan: eb_sender,
        };
        let meta = flusher.meta.clone();
        let data_dir = "./test_flusher/data";
        // prepare tree
        let shard_id = 0;
        let (entry_file_size, twig_file_size);
        let mut tree = Tree::new(
            0,
            SMALL_BUFFER_SIZE as usize,
            DEFAULT_FILE_SIZE,
            data_dir.to_string(),
            ".test".to_string(),
            true,
            cipher.clone(),
        );
        {
            let mut meta = meta.write_arc();
            let mut bz = [0u8; DEFAULT_ENTRY_SIZE];
            for sn in 0..SENTRY_COUNT {
                let e = sentry_entry(shard_id, sn as u64, &mut bz[..]);
                tree.append_entry(&e).unwrap();
            }
            let n_list = tree.flush_files(0, 0);
            let n_list = tree.upper_tree.evict_twigs(n_list, 0, 0);
            tree.upper_tree
                .sync_upper_nodes(n_list, tree.youngest_twig_id);
            check_hash_consistency(&tree);
            (entry_file_size, twig_file_size) = tree.get_file_sizes();
            meta.set_entry_file_size(shard_id, entry_file_size);
            meta.set_twig_file_size(shard_id, twig_file_size);
            meta.set_next_serial_num(shard_id, SENTRY_COUNT as u64);
            meta.insert_extra_data(0, "".to_owned());
            meta.commit();
        }

        let entry_file = tree.entry_file_wr.entry_file.clone();
        let fs = FlusherShard::new(tree, 0, shard_id);

        flusher.shards.push(Box::new(fs));
        let _tree_p = &mut flusher.shards[0].tree as *mut Tree;
        let (mut u_eb_wr, u_eb_rd) = entrybuffer::new(entry_file_size, SMALL_BUFFER_SIZE as usize);
        flusher.shards[shard_id].buf_read = Some(u_eb_rd);
        // prepare entry
        let e0 = Entry {
            key: "Key0Key0Key0Key0Key0Key0Key0Key0Key0".as_bytes(),
            value: "Value0Value0Value0Value0Value0Value0".as_bytes(),
            next_key_hash: [1; 32].as_slice(),
            version: 1,
            serial_number: SENTRY_COUNT as u64,
        };
        let mut buf = [0; 1024];
        let bz0 = entry_to_bytes(&e0, &[], &mut buf);
        let pos0 = u_eb_wr.append(&e0, &[]);
        u_eb_wr.end_block(0, 0, SENTRY_COUNT as u64);
        let _handler = thread::spawn(move || {
            flusher.flush(1);
        });
        sleep(time::Duration::from_secs(3));
        let mut buf = [0; 1024];
        let size0 = entry_file.read_entry(pos0, &mut buf);
        assert_eq!(buf[..size0], *bz0.bz);

        let meta = meta.read_arc();
        let oldest_active_sn = meta.get_oldest_active_sn(shard_id);
        let oldest_active_twig_id = oldest_active_sn >> TWIG_SHIFT;
        let youngest_twig_id = meta.get_youngest_twig_id(shard_id);
        let edge_nodes = bytes_to_edge_nodes(&meta.get_edge_nodes(shard_id));
        let (last_pruned_twig_id, ef_prune_to) = meta.get_last_pruned_twig(shard_id);
        let root = meta.get_root_hash(shard_id);
        let entryfile_size = meta.get_entry_file_size(shard_id);
        let twigfile_size = meta.get_twig_file_size(shard_id);
        let (tree, recovered_root) = recover_tree(
            0,
            SMALL_BUFFER_SIZE as usize,
            DEFAULT_FILE_SIZE as usize,
            true,
            data_dir.to_string(),
            ".test".to_string(),
            &edge_nodes,
            last_pruned_twig_id,
            ef_prune_to,
            oldest_active_twig_id,
            youngest_twig_id,
            &[entryfile_size, twigfile_size],
            cipher,
        );
        assert_eq!(recovered_root, root);
        check_hash_consistency(&tree);
        let mut proof_path = tree.get_proof(SENTRY_COUNT as u64).unwrap();
        check_proof(&mut proof_path).unwrap();
    }
}
