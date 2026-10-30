use crate::{
    beatree::{
        allocator::{PageNumber, Store, StoreReader},
        branch::{self, node::BranchNode, BRANCH_NODE_BODY_SIZE, BRANCH_NODE_SIZE},
        leaf::{
            self,
            node::{LeafNode, MAX_LEAF_VALUE_SIZE},
        },
        leaf_cache::LeafCache,
        ops::{
            self,
            bit_ops::separate,
            update::{
                branch_stage::BranchStageOutput,
                branch_updater::tests::make_branch_until,
                get_key,
                leaf_stage::{self, LeafStageOutput},
                BRANCH_MERGE_THRESHOLD, LEAF_MERGE_THRESHOLD,
            },
        },
        Index, ValueChange,
    },
    io::{start_test_io_pool, IoPool, PagePool},
};
use lazy_static::lazy_static;
use quickcheck::{Arbitrary, Gen, QuickCheck, TestResult};
use rand::{Rng, SeedableRng};
use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::fs::File;
use std::sync::Arc;
use threadpool::ThreadPool;

const LEAF_STAGE_INITIAL_CAPACITY: usize = 700;
const BRANCH_STAGE_INITIAL_CAPACITY: usize = 50_000;
const LEAF_STAGE_CHANGESET_AVG_SIZE: usize = 300;
const BRANCH_STAGE_CHANGESET_AVG_SIZE: usize = 100;
const MAX_TESTS: u64 = 50;

// Required to increase reproducibility
lazy_static! {
    static ref SEED: [u8; 16] = {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("no time?")
            .as_nanos()
            .to_le_bytes()[0..16]
            .try_into()
            .unwrap()
    };
    static ref KEYS: Vec<[u8; 32]> = rand_keys(LEAF_STAGE_INITIAL_CAPACITY);
    static ref SEPARATORS: Vec<[u8; 32]> = {
        let mut separators = vec![[0; 32]];
        separators.extend(
            rand_keys(BRANCH_STAGE_INITIAL_CAPACITY)
                .windows(2)
                .map(|w| separate(&w[0], &w[1])),
        );
        separators
    };
    static ref PAGE_POOL: PagePool = PagePool::new();
    static ref IO_POOL: IoPool = start_test_io_pool(3, PAGE_POOL.clone());
    static ref TREE_DATA: TreeData = init_beatree();
    static ref BBN_INDEX: Index = init_bbn_index(&SEPARATORS);
    static ref THREAD_POOL: ThreadPool =
        ThreadPool::with_name("beatree-update-test".to_string(), 64);
}

fn rand_keys(n: usize) -> Vec<[u8; 32]> {
    let mut rng = rand_pcg::Lcg64Xsh32::from_seed(*SEED);
    let mut items = BTreeSet::new();
    while items.len() < n {
        let mut key = [0; 32];
        rng.fill(&mut key);
        items.insert(key);
    }
    items.into_iter().collect()
}

struct TreeData {
    ln_fd: Arc<File>,
    ln_freelist_pn: u32,
    ln_bump: u32,
    init_items: BTreeMap<[u8; 32], Vec<u8>>,
    bbn_index: Index,
}

impl TreeData {
    fn leaf_store(&self) -> Store {
        Store::open(
            &PAGE_POOL,
            self.ln_fd.clone(),
            PageNumber(self.ln_bump),
            Some(PageNumber(self.ln_freelist_pn)),
        )
        .unwrap()
    }
}

// Initialize the beatree utilizing the keys present in KEYS and creating random value sizes.
//
// This initialization is required for three main reasons:
// + to make quickcheck tests faster, because otherwise, each iteration the db should have been filled up
// + to make reproducibility possible, knowing the seed with which the keys and value sizes are generated
// makes it possible to hardcode them once a bug is found
// + to let the quickcheck input shrinking work better, if each iteration works on the same underlying
// db then the shrinking could be more effective at finding the smallest input that makes the error occur
fn init_beatree() -> TreeData {
    let mut rng = rand_pcg::Lcg64Xsh32::from_seed(*SEED);

    let ln_fd = tempfile::tempfile().unwrap();
    let bbn_fd = tempfile::tempfile().unwrap();
    ln_fd.set_len(BRANCH_NODE_SIZE as u64).unwrap();
    bbn_fd.set_len(BRANCH_NODE_SIZE as u64).unwrap();

    let ln_fd = Arc::new(ln_fd);
    let bbn_fd = Arc::new(bbn_fd);

    let initial_items: BTreeMap<[u8; 32], Vec<u8>> = KEYS
        .iter()
        .cloned()
        .map(|key| (key, vec![170u8; rng.gen_range(500..MAX_LEAF_VALUE_SIZE)]))
        .collect();

    let leaf_store = Store::open(&PAGE_POOL, ln_fd.clone(), PageNumber(1), None).unwrap();

    let bbn_store = Store::open(&PAGE_POOL, bbn_fd.clone(), PageNumber(1), None).unwrap();

    let (sync_data, bbn_index, _) = super::update(
        initial_items
            .clone()
            .into_iter()
            .map(|(k, v)| (k, ValueChange::Insert(v)))
            .collect(),
        Index::default(),
        LeafCache::new(1, 1024),
        leaf_store,
        bbn_store,
        PAGE_POOL.clone(),
        IO_POOL.make_handle(),
        THREAD_POOL.clone(),
        1,
    )
    .unwrap();

    ln_fd.sync_all().unwrap();
    bbn_fd.sync_all().unwrap();

    TreeData {
        ln_fd,
        ln_freelist_pn: sync_data.ln_freelist_pn,
        ln_bump: sync_data.ln_bump,
        init_items: initial_items,
        bbn_index,
    }
}

#[derive(Clone, Debug, Eq, PartialEq, PartialOrd, Ord)]
struct Key {
    inner: [u8; 32],
}

// required to let quickcheck generate arbitrary keys as arguments for the tests
impl Arbitrary for Key {
    fn arbitrary(g: &mut Gen) -> Key {
        let mut key = [0; 32];
        for k in key.iter_mut() {
            *k = u8::arbitrary(g);
        }
        Key { inner: key }
    }
}

#[derive(Clone, Debug)]
struct StageInputs {
    insertions: BTreeMap<Key, u16>,
    deletions: Vec<u16>,
    no_shrinking: bool,
}

impl Arbitrary for StageInputs {
    fn arbitrary(g: &mut Gen) -> Self {
        // If NO_STAGES_SHRINKING is specified as true,
        // it stops the shrinking process from occurring,
        // returning immediately if an error is found.
        let no_shrinking = std::env::var("NO_STAGES_SHRINKING")
            .ok()
            .map(|no_shrinking| no_shrinking.to_lowercase() == "true")
            .unwrap_or(false);

        Self {
            insertions: BTreeMap::<Key, u16>::arbitrary(g),
            deletions: Vec::<u16>::arbitrary(g),
            no_shrinking,
        }
    }

    fn shrink(&self) -> Box<dyn Iterator<Item = Self>> {
        if self.no_shrinking {
            return quickcheck::empty_shrinker();
        }

        Box::new(self.insertions.shrink().zip(self.deletions.shrink()).map(
            |(insertions, deletions)| Self {
                insertions,
                deletions,
                no_shrinking: false,
            },
        ))
    }
}

fn leaf_page_numbers(
    bbn_index: &Index,
    keys: impl IntoIterator<Item = [u8; 32]>,
) -> BTreeSet<PageNumber> {
    let mut page_numbers = BTreeSet::new();
    for key in keys {
        let Some((_, branch)) = bbn_index.lookup(key) else {
            continue;
        };
        let Some((_, leaf_pn)) = ops::search_branch(&branch, key) else {
            continue;
        };

        page_numbers.insert(leaf_pn);
    }

    page_numbers
}

// given a changeset execute the leaf stage on top of a pre-initialized nomt-db
fn exec_leaf_stage(
    commit_concurrency: usize,
    changeset: BTreeMap<[u8; 32], ValueChange>,
) -> (LeafStageOutput, BTreeSet<PageNumber>) {
    let leaf_store = TREE_DATA.leaf_store();
    let leaf_reader = StoreReader::new(leaf_store.clone(), PAGE_POOL.clone());
    let (leaf_writer, leaf_finisher) = leaf_store.start_sync();

    let bbn_index = &TREE_DATA.bbn_index;
    let leaf_cache = LeafCache::new(commit_concurrency, 1024);
    let leaf_page_numbers = leaf_page_numbers(&bbn_index, changeset.keys().cloned());

    let io_handle = IO_POOL.make_handle();
    let leaf_stage_output = super::leaf_stage::run(
        bbn_index,
        leaf_cache,
        leaf_reader,
        leaf_writer,
        io_handle.clone(),
        changeset.into_iter().collect(),
        THREAD_POOL.clone(),
        commit_concurrency,
    )
    .unwrap();

    // we don't actually write the free-list pages so the store is effectively clean.
    let _ = leaf_finisher.finish(&PAGE_POOL, Vec::new()).unwrap();

    for _ in 0..leaf_stage_output.submitted_io {
        io_handle.recv().unwrap();
    }

    TREE_DATA.ln_fd.sync_all().unwrap();

    (leaf_stage_output, leaf_page_numbers)
}

fn is_valid_leaf_stage_output(
    output: LeafStageOutput,
    mut used_page_numbers: BTreeSet<PageNumber>,
    deletions: BTreeSet<[u8; 32]>,
    insertions: BTreeMap<[u8; 32], Vec<u8>>,
) -> bool {
    if output.leaf_changeset.is_empty() {
        return true;
    }

    let leaf_store = TREE_DATA.leaf_store();
    let leaf_reader = StoreReader::new(leaf_store.clone(), PAGE_POOL.clone());

    for page_number in output.freed_pages {
        used_page_numbers.remove(&page_number);
    }

    let mut expected_values = TREE_DATA.init_items.clone();
    expected_values.extend(insertions.clone());
    expected_values.retain(|k, _| !deletions.contains(k));

    let mut found_underfull_leaf = false;
    for (separator, new_pn) in output.leaf_changeset.into_iter() {
        if separator == [0; 32] && new_pn.is_none() {
            // First leaf with separator of all 0 cannot be eliminated.
            return false;
        }

        let Some(new_pn) = new_pn else { continue };
        let page = leaf_reader.query(new_pn);
        let leaf_node = LeafNode { inner: page };

        let n = leaf_node.n();

        let mut value_size_sum = 0;

        // each leaf must contain all the keys that are expected in the range
        // between its first and last key
        let first = leaf_node.key(0);
        let last = leaf_node.key(n - 1);
        if first > last {
            return false;
        }
        let mut expected = expected_values.range(first..=last);

        for i in 0..n {
            let key = leaf_node.key(i);
            if deletions.contains(&key) {
                return false;
            }

            let value = leaf_node.value(i).0;
            value_size_sum += value.len();

            let Some((expected_key, expected_value)) = expected.next() else {
                return false;
            };

            if key != *expected_key || value != expected_value {
                return false;
            }
        }

        if expected.next().is_some() {
            return false;
        }

        // all new leaves must respect the half-full requirement except for the last one
        if leaf::node::body_size(n, value_size_sum) < LEAF_MERGE_THRESHOLD {
            if found_underfull_leaf == true {
                return false;
            }
            found_underfull_leaf = true;
        }
    }

    // all preload leaves must be deleted to be a valid output
    used_page_numbers.is_empty()
}

// given an u16 value rescale it proportionally to the new upper bound provided
fn rescale(init: u16, lower_bound: usize, upper_bound: usize) -> usize {
    ((init as f64 / u16::MAX as f64) * ((upper_bound - 1 - lower_bound) as f64)).round() as usize
        + lower_bound
}

// insertions is a map of arbitrary keys associated with value sizes, while deletions
// is a vector of numbers that will be used to index the vector of already present keys
fn leaf_stage_inner(input: StageInputs) -> TestResult {
    let deletions: BTreeMap<_, _> = input
        .deletions
        .into_iter()
        // rescale deletions to contain indexes over only alredy present items in the db
        .map(|d| rescale(d, 0, KEYS.len()))
        .map(|index| (KEYS[index], ValueChange::Delete))
        .collect();

    let insertions: BTreeMap<_, _> = input
        .insertions
        .into_iter()
        // rescale raw_size to be between 0 and MAX_LEAF_VALUE_SIZE
        .map(|(k, raw_size)| (k, rescale(raw_size, 1, MAX_LEAF_VALUE_SIZE)))
        .map(|(k, size)| (k.inner, ValueChange::Insert(vec![170; size])))
        .collect();

    let mut changeset: BTreeMap<[u8; 32], ValueChange> = insertions.clone();
    changeset.extend(deletions.clone());

    let (leaf_stage_output, prior_leaf_page_numbers) = exec_leaf_stage(64, changeset);

    TestResult::from_bool(is_valid_leaf_stage_output(
        leaf_stage_output,
        prior_leaf_page_numbers,
        deletions.into_iter().map(|(k, _)| k).collect(),
        insertions
            .into_iter()
            .map(|(k, v)| (k, v.as_option().unwrap().to_vec()))
            .collect(),
    ))
}

#[test]
fn leaf_stage() {
    let test_result = std::panic::catch_unwind(|| {
        QuickCheck::new()
            .gen(Gen::new(LEAF_STAGE_CHANGESET_AVG_SIZE))
            .max_tests(MAX_TESTS)
            .quickcheck(leaf_stage_inner as fn(_) -> TestResult)
    });

    if let Err(cause) = test_result {
        eprintln!("lead_stage failed with seed: {:?}", *SEED);
        std::panic::resume_unwind(cause);
    }
}

// Initialize a bbn_index using the provided separators.
// The reasons why this initialization is required are the same as those for `init_beatree`
// It is also used by `enforce_first_leaf_separator` to avoid initializing the entire beatree.
fn init_bbn_index(separators: &[[u8; 32]]) -> Index {
    let mut rng = rand_pcg::Lcg64Xsh32::from_seed(*SEED);

    let mut bbn_index = Index::default();
    let mut used_separators = 0;
    let mut bbn_pn = 0;

    while used_separators < separators.len() {
        let body_size_target = rng.gen_range(BRANCH_MERGE_THRESHOLD..BRANCH_NODE_BODY_SIZE);

        let branch_node = make_branch_until(
            &mut separators[used_separators..].iter().cloned(),
            body_size_target,
            bbn_pn,
        );

        let separator = get_key(&branch_node, 0);
        used_separators += branch_node.n() as usize;
        bbn_index.insert(separator, branch_node);
        bbn_pn += 1;
    }

    bbn_index
}

fn is_valid_branch_stage_output(
    changed_branches: BTreeMap<[u8; 32], Arc<BranchNode>>,
    branch_stage_output: BranchStageOutput,
    mut bbn_page_numbers: BTreeSet<u32>,
    insertions: BTreeSet<[u8; 32]>,
    deletions: BTreeSet<[u8; 32]>,
) -> bool {
    let mut expected_values: BTreeSet<[u8; 32]> = SEPARATORS.iter().cloned().collect();
    expected_values.extend(insertions.clone());
    expected_values.retain(|k| !deletions.contains(k));

    let mut found_underfull_branch = false;
    for (_, branch_node) in changed_branches.into_iter() {
        if bbn_page_numbers.contains(&branch_node.bbn_pn()) {
            return false;
        }

        let n = branch_node.n() as usize;

        let prefix_bit_len = branch_node.raw_prefix().1;
        let total_separator_lengths = branch_node.view().cell(n - 1);

        // all new bbns must respect the half-full requirement except for the last one
        if branch::node::body_size(prefix_bit_len, total_separator_lengths, n)
            < BRANCH_MERGE_THRESHOLD
        {
            if found_underfull_branch {
                return false;
            }
            found_underfull_branch = true;
        }

        let first = get_key(&branch_node, 0);
        let last = get_key(&branch_node, n - 1);
        if first > last {
            return false;
        }
        let mut expected = expected_values.range(first..=last);

        for i in 0..n {
            let key = get_key(&branch_node, i);
            if deletions.contains(&key) {
                return false;
            }

            let Some(expected_key) = expected.next() else {
                return false;
            };

            if key != *expected_key {
                return false;
            }
        }

        if expected.next().is_some() {
            return false;
        }
    }

    for freed_page in branch_stage_output.freed_pages {
        bbn_page_numbers.remove(&freed_page.0);
    }

    bbn_page_numbers.is_empty()
}

// insertions is a map of arbitrary keys associated with a PageNumber, while deletions
// is a vector of numbers that will be used to index the vector of already present SEPARATORS
fn branch_stage_inner(input: StageInputs) -> TestResult {
    let bbn_index = BBN_INDEX.clone();

    let deletions: BTreeMap<_, _> = input
        .deletions
        .into_iter()
        // rescale deletions to contain indexes over only alredy present items in the db
        .map(|d| rescale(d, 0, SEPARATORS.len()))
        .map(|index| (SEPARATORS[index], None))
        .collect();

    let insertions: BTreeMap<_, _> = input
        .insertions
        .into_iter()
        .map(|(k, raw_pn)| (k.inner, Some(PageNumber(raw_pn as u32))))
        .collect();

    let bbn_fd = tempfile::tempfile().unwrap();
    bbn_fd.set_len(BRANCH_NODE_SIZE as u64).unwrap();
    let bbn_fd = Arc::new(bbn_fd);

    let bbn_store = Store::open(
        &PAGE_POOL,
        bbn_fd.clone(),
        PageNumber(SEPARATORS.len() as u32),
        None,
    )
    .unwrap();

    let mut changeset: BTreeMap<[u8; 32], Option<PageNumber>> = insertions.clone();
    changeset.extend(deletions.clone());

    let bbn_page_numbers: BTreeSet<_> = changeset
        .iter()
        .map(|(key, _)| bbn_index.lookup(*key).unwrap().1.bbn_pn())
        .collect();

    let mut new_bbn_index = bbn_index.clone();
    let (bbn_writer, bbn_finisher) = bbn_store.start_sync();

    let io_handle = IO_POOL.make_handle();
    let branch_stage_output = super::branch_stage::run(
        &mut new_bbn_index,
        bbn_writer,
        PAGE_POOL.clone(),
        io_handle.clone(),
        changeset.into_iter().collect(),
        THREAD_POOL.clone(),
        64,
    )
    .unwrap();

    let old_pages = bbn_index.into_iter().collect::<BTreeMap<_, _>>();

    let mut changed_branches = BTreeMap::new();
    for (separator, node) in new_bbn_index.into_iter() {
        match old_pages.get(&separator) {
            None => {
                changed_branches.insert(separator, node);
            }
            Some(prev) if prev.bbn_pn() != node.bbn_pn() => {
                changed_branches.insert(separator, node);
            }
            Some(_) => {}
        }
    }

    // we don't actually write the free-list pages so the store is effectively clean.
    let _ = bbn_finisher.finish(&PAGE_POOL, Vec::new()).unwrap();

    for _ in 0..branch_stage_output.submitted_io {
        io_handle.recv().unwrap();
    }

    bbn_fd.sync_all().unwrap();

    TestResult::from_bool(is_valid_branch_stage_output(
        changed_branches,
        branch_stage_output,
        bbn_page_numbers,
        insertions.keys().cloned().collect(),
        deletions.keys().cloned().collect(),
    ))
}

#[test]
fn branch_stage() {
    let test_result = std::panic::catch_unwind(|| {
        QuickCheck::new()
            .gen(Gen::new(BRANCH_STAGE_CHANGESET_AVG_SIZE))
            .max_tests(MAX_TESTS)
            .quickcheck(branch_stage_inner as fn(_) -> TestResult)
    });

    if let Err(cause) = test_result {
        eprintln!("branch_stage failed with seed: {:?}", *SEED);
        std::panic::resume_unwind(cause);
    }
}

#[test]
fn enforce_first_leaf_separator() {
    let mut separators = vec![[0; 32]];
    let keys = rand_keys(9);
    separators.extend(keys.windows(2).map(|w| separate(&w[0], &w[1])));
    // NOTE: The test relies entirely on the fact that `init_bbn_index`
    // uses `make_branch_until`, which assigns page numbers in ascending order starting from 0.
    let bbn_index = init_bbn_index(&separators);

    // nothing changes, first leaf not deleted
    let mut leaf_changeset = vec![([1; 32], Some(PageNumber(1))), ([2; 32], None)];
    let prev_stage = leaf_changeset.clone();
    leaf_stage::enforce_first_leaf_separator(&mut leaf_changeset, &bbn_index);
    assert_eq!(leaf_changeset, prev_stage);

    // nothing changes, all leaves deleted
    let mut leaf_changeset = separators.iter().map(|s| (*s, None)).collect::<Vec<_>>();
    let prev_stage = leaf_changeset.clone();
    leaf_stage::enforce_first_leaf_separator(&mut leaf_changeset, &bbn_index);
    assert_eq!(leaf_changeset, prev_stage);

    // The new first leaf is within the previous leaves, all deleted in changeset
    let n_take = 5;
    let mut leaf_changeset = separators
        .iter()
        .map(|s| (*s, None))
        .take(n_take)
        .collect::<Vec<_>>();
    leaf_stage::enforce_first_leaf_separator(&mut leaf_changeset, &bbn_index);

    // new first leaf
    assert_eq!(leaf_changeset[0].0, [0; 32]);
    assert_eq!(leaf_changeset[0].1, Some(PageNumber(n_take as u32)));
    // All others are expected to be deleted leaves,
    // in particular, the previous separator associated with the new first leaf is deleted.
    for (_, leaf_pn) in &leaf_changeset[1..] {
        assert!(leaf_pn.is_none());
    }
    assert_eq!(leaf_changeset.len(), n_take + 1);

    // The new first leaf is within the previous leaves,
    // some deleted before finding it
    let n_take = 5;
    let mut leaf_changeset = separators
        .iter()
        .map(|s| (*s, None))
        .take(n_take)
        .collect::<Vec<_>>();
    // this is the new leaf with a bigger separator than one of the previous
    leaf_changeset.push((keys[n_take + 1], Some(PageNumber(15))));
    leaf_stage::enforce_first_leaf_separator(&mut leaf_changeset, &bbn_index);

    // new first leaf
    assert_eq!(leaf_changeset[0].0, [0; 32]);
    assert_eq!(leaf_changeset[0].1, Some(PageNumber(n_take as u32)));
    // + 1 = the leaf we pushed
    // + 1 = deletion of the separator of the new first leaf
    assert_eq!(leaf_changeset.len(), n_take + 2);

    // The new first leaf is a new leaf, some deleted before finding it
    let n_take = 5;
    let mut leaf_changeset = separators
        .iter()
        .map(|s| (*s, None))
        .take(n_take)
        .collect::<Vec<_>>();
    // this is the new leaf with a smaller separator than one of the previous
    leaf_changeset.push((keys[n_take - 1], Some(PageNumber(15))));
    leaf_stage::enforce_first_leaf_separator(&mut leaf_changeset, &bbn_index);

    // new first leaf
    assert_eq!(leaf_changeset[0].0, [0; 32]);
    assert_eq!(leaf_changeset[0].1, Some(PageNumber(15)));
    //  leaf_changeset len in n_take + 1, but the item associated
    //  to the new first leaf is removed
    assert_eq!(leaf_changeset.len(), n_take);

    // The new first leaf is an updated version of a previous leaf,
    // something deleted before finding it
    let n_take = 5;
    let mut leaf_changeset = separators
        .iter()
        .map(|s| (*s, None))
        .take(n_take)
        .collect::<Vec<_>>();
    // this is the new leaf with a smaller separator than one of the previous
    leaf_changeset.push((separators[n_take], Some(PageNumber(15))));
    leaf_stage::enforce_first_leaf_separator(&mut leaf_changeset, &bbn_index);

    // new first leaf
    assert_eq!(leaf_changeset[0].0, [0; 32]);
    assert_eq!(leaf_changeset[0].1, Some(PageNumber(15)));
    assert_eq!(leaf_changeset[n_take].1, None);
    assert_eq!(leaf_changeset.len(), n_take + 1);
}
