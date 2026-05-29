use rayon::str;
use smallvec::SmallVec;

const DATA_LEN: usize = 100;
const BITS: usize = usize::BITS as usize;

struct TaskIdSetPerBlock {
    task_id_high: i64,
    data: [usize; DATA_LEN],
}

impl TaskIdSetPerBlock {
    fn insert(&mut self, index: usize) {
        let word = index / BITS;
        let offset = index % BITS;
        self.data[word % DATA_LEN] |= 1 << offset;
    }

    fn remove(&mut self, index: usize) {
        let word = index / BITS;
        let offset = index % BITS;
        self.data[word % DATA_LEN] &= !(1 << offset);
    }
}

struct TaskIdSet {
    next_task_id_excude_end_block: i64,
    max_task_id_excude_end_block: i64,
    datas: SmallVec<[TaskIdSetPerBlock; 2]>,
    max_task_id_high: i64,
}

// impl TaskIdSet {
//     fn insert(&mut self, task_id: i64) {
//         let word = bit / BITS;
//         let offset = bit % BITS;
//         self.data[word] |= 1 << offset;
//     }

//     fn remove(&mut self, height_and_index: i64) {
//         // let word = bit / BITS;
//         // let offset = bit % BITS;
//         // self.data[word] &= !(1 << offset);
//     }
// }
