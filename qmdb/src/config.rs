pub const COMPACT_THRES: i64 = 20000000;
pub const UTILIZATION_RATIO: i64 = 7;
pub const UTILIZATION_DIV: i64 = 10;

const TASK_CHAN_SIZE: usize = 200000;
const PREFETCHER_THREAD_COUNT: usize = 512;
const URING_SIZE: u32 = 1024;
const URING_COUNT: usize = 32;
const SUB_ID_CHAN_SIZE: usize = 20000;

pub struct Config {
    pub dir: String,
    pub wrbuf_size: usize,
    pub file_segment_size: usize,
    pub with_twig_file: bool,
    pub aes_keys: Option<[u8; 96]>,
    pub compact_thres: i64,
    pub utilization_ratio: i64,
    pub utilization_div: i64,
    pub task_chan_size: usize,
    pub prefetcher_thread_count: usize,
    pub uring_count: usize,
    pub uring_size: u32,
    pub sub_id_chan_size: usize,
    pub shard_count: usize,
    pub compactor_thread_count: usize,
    pub updater_thread_count: usize,
    pub numa_affinity: bool,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            dir: "default".to_string(),
            wrbuf_size: 8 * 1024 * 1024,           //8MB
            file_segment_size: 1024 * 1024 * 1024, // 1GB
            with_twig_file: false,
            aes_keys: None,
            compact_thres: COMPACT_THRES,
            utilization_ratio: UTILIZATION_RATIO,
            utilization_div: UTILIZATION_DIV,
            task_chan_size: TASK_CHAN_SIZE,
            prefetcher_thread_count: PREFETCHER_THREAD_COUNT,
            uring_count: URING_COUNT,
            uring_size: URING_SIZE,
            sub_id_chan_size: SUB_ID_CHAN_SIZE,
            shard_count: 16,
            compactor_thread_count: 16,
            updater_thread_count: 16,
            numa_affinity: false,
        }
    }
}

impl Config {
    pub fn new(
        dir: &str,
        wrbuf_size: usize,
        file_segment_size: usize,
        with_twig_file: bool,
        aes_keys: Option<[u8; 96]>,
        compact_thres: i64,
        utilization_ratio: i64,
        utilization_div: i64,
        task_chan_size: usize,
        prefetcher_thread_count: usize,
        uring_count: usize,
        uring_size: u32,
        sub_id_chan_size: usize,
    ) -> Self {
        Self {
            dir: dir.to_string(),
            wrbuf_size,
            file_segment_size,
            with_twig_file,
            aes_keys,
            compact_thres,
            utilization_ratio,
            utilization_div,
            task_chan_size,
            prefetcher_thread_count,
            uring_count,
            uring_size,
            sub_id_chan_size,
            shard_count: 16,
            compactor_thread_count: 16,
            updater_thread_count: 16,
            numa_affinity: false,
        }
    }

    pub fn from_dir(dir: &str) -> Self {
        let mut config = Config::default();
        config.dir = dir.to_string();
        config
    }

    pub fn from_dir_and_compact_opt(
        dir: &str,
        compact_thres: i64,
        utilization_ratio: i64,
        utilization_div: i64,
    ) -> Self {
        let mut config = Config::default();
        config.dir = dir.to_string();
        config.compact_thres = compact_thres;
        config.utilization_ratio = utilization_ratio;
        config.utilization_div = utilization_div;
        config
    }

    pub fn set_aes_keys(&mut self, keys: [u8; 96]) {
        self.aes_keys = Some(keys);
    }

    pub fn set_with_twig_file(&mut self, with_twig_file: bool) {
        self.with_twig_file = with_twig_file;
    }
}
