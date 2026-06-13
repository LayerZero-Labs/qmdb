extern crate structopt;
#[macro_use]
extern crate strum_macros;

pub use structopt::StructOpt;

#[derive(Debug, StructOpt)]
#[structopt(about = "Authenticated Storage Benchmarks", rename_all = "kebab-case")]
pub struct Options {
    #[structopt(short = "a", parse(try_from_str = parse_algo), long)]
    pub algorithm: AuthAlgo,

    #[structopt(short = "b", parse(try_from_str = parse_backend), long, default_value="rocksdb")]
    pub backend: Backend,

    #[structopt(short = "k", long, parse(try_from_str = parse_num), default_value = "100000")]
    pub total_keys: usize,

    #[structopt(long, default_value = "64")]
    pub seed: u64,

    #[structopt(long, default_value = "1500")]
    pub cache_size: u64,

    #[structopt(long)]
    pub max_time: Option<u64>,

    #[structopt(long)]
    pub max_epoch: Option<usize>,

    #[structopt(long, default_value = "2")]
    pub report_epoch: usize,

    #[structopt(long, default_value = "100")]
    pub profile_epoch: usize,

    #[structopt(long, default_value = "50000")]
    pub epoch_size: usize,

    #[structopt(long = "pprof-report-to")]
    pub report_dir: Option<String>,

    #[structopt(long = "db", default_value = "./__benchmarks")]
    pub db_dir: String,

    #[structopt(long = "trace", default_value = "./trace")]
    pub trace_dir: String,

    #[structopt(long, help = "Use real trace")]
    pub real_trace: bool,

    #[structopt(long, help = "Disable backend stat")]
    pub no_stat: bool,

    #[structopt(long, help = "Output the usage of memory")]
    pub stat_mem: bool,

    #[structopt(long, help = "No warmup")]
    pub no_warmup: bool,

    #[structopt(long, help = "Enable print root")]
    pub print_root: bool,

    #[structopt(long)]
    pub warmup_to: Option<String>,

    #[structopt(long)]
    pub warmup_from: Option<String>,

    #[structopt(long)]
    pub shards: Option<usize>,
}

impl Options {
    fn warmup_dir(&self, input: &str) -> String {
        let task_code = if !self.real_trace {
            format!("{:e}", self.total_keys)
        } else {
            "real".into()
        };
        if self.algorithm != AuthAlgo::LVMT || self.shards.is_none() {
            format!("{}/{:?}_{}/", input, self.algorithm, task_code)
        } else {
            format!("{}/LVMT{}_{}/", input, self.shards.unwrap(), task_code)
        }
    }
    pub fn settings(&self) -> String {
        format!("{:?},{:e}", self.algorithm, self.total_keys)
    }
    pub fn warmup_to(&self) -> Option<String> {
        self.warmup_to.as_ref().map(|x| self.warmup_dir(x))
    }
    pub fn warmup_from(&self) -> Option<String> {
        self.warmup_from.as_ref().map(|x| self.warmup_dir(x))
    }

    pub fn num_cols(&self) -> u32 {
        match self.algorithm {
            AuthAlgo::LVMT => 3,
            _ => 1,
        }
    }
}

#[derive(Debug, Eq, PartialEq, EnumString)]
#[strum(serialize_all = "lowercase")]
pub enum AuthAlgo {
    RAW,
    AMT(usize),
    LVMT,
    MPT,
    LMPTS,
    RAIN,
}

fn parse_algo(s: &str) -> Result<AuthAlgo, String> {
    if s.len() >= 4 && &s[0..4] == "amt" {
        let depth = s[4..].parse::<usize>().map_err(|x| x.to_string())?;
        return Ok(AuthAlgo::AMT(depth));
    }
    return Ok(match s {
        "raw" => AuthAlgo::RAW,
        "lvmt" => AuthAlgo::LVMT,
        "mpt" => AuthAlgo::MPT,
        "lmpts" => AuthAlgo::LMPTS,
        "rain" => AuthAlgo::RAIN,
        _ => {
            return Err("Unrecognized algorithm".into());
        }
    });
}

fn parse_num(s: &str) -> Result<usize, String> {
    let base = match s
        .chars()
        .rev()
        .next()
        .ok_or::<String>("empty input".into())?
    {
        'k' | 'K' => 1_000,
        'm' | 'M' => 1_000_000,
        'g' | 'G' => 1_000_000_000,
        _ => 1,
    };
    let num = if base > 1 {
        let mut chars = s.chars();
        chars.next_back();
        chars.as_str()
    } else {
        s
    };
    Ok(base * num.parse::<usize>().map_err(|x| x.to_string())?)
}

#[derive(Debug, Eq, PartialEq, EnumString)]
#[strum(serialize_all = "lowercase")]
pub enum Backend {
    RocksDB,
    InMemoryDB,
    MDBX,
}

fn parse_backend(s: &str) -> Result<Backend, String> {
    return Ok(match s {
        "rocksdb" => Backend::RocksDB,
        "memory" => Backend::InMemoryDB,
        "mdbx" => Backend::MDBX,
        _ => {
            return Err("Unrecognized backend".into());
        }
    });
}
