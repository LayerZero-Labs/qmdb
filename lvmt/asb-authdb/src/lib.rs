mod amt;
#[cfg(feature = "lmpts")]
mod lmpts;
mod lvmt;
mod mpt;
mod rain_mpt;
mod raw;

use lvmt::LvmtCounter;
use mpt::MptCounter;

use asb_options::{AuthAlgo, Options};
use asb_profile::CounterTrait;
use asb_profile::{Counter, Reporter};
use authdb::AuthDB;
use kvdb::KeyValueDB;
use std::sync::Arc;

fn open_lmpts(dir: &str) -> Box<dyn AuthDB> {
    #[cfg(feature = "lmpts")]
    {
        Box::new(lmpts::new(dir))
    }
    #[cfg(not(feature = "lmpts"))]
    {
        let _ = dir;
        panic!("LMPTs can only work with feature asb-backend!")
    }
}

pub fn new<'a>(backend: Arc<dyn KeyValueDB>, opts: &'a Options) -> (Box<dyn AuthDB>, Reporter<'a>) {
    let (db, counter): (Box<dyn AuthDB>, Box<dyn CounterTrait>) = match opts.algorithm {
        AuthAlgo::RAW => (Box::new(raw::new(backend)), Box::new(Counter::default())),
        AuthAlgo::LVMT => (
            Box::new(lvmt::new(backend, opts)),
            Box::new(LvmtCounter::default()),
        ),
        AuthAlgo::MPT => {
            let mpt_db = mpt::new(backend, opts);
            let counter = MptCounter::from_mpt_db(&mpt_db);
            (Box::new(mpt_db), Box::new(counter))
        }
        AuthAlgo::LMPTS => (open_lmpts(&opts.db_dir), Box::new(Counter::default())),
        AuthAlgo::AMT(x) => {
            let authdb = exaust_construct!(x, backend, opts, 20, 21, 22, 23, 24, 25, 26, 27, 28);
            (authdb, Box::new(Counter::default()))
        }
        AuthAlgo::RAIN => (
            Box::new(rain_mpt::new(backend)),
            Box::new(Counter::default()),
        ),
    };

    let mut reporter = Reporter::new(opts);
    reporter.set_counter(counter);

    return (db, reporter);
}

macro_rules! exaust_construct {
    ($input: ident, $backend: ident, $opts: ident, $idx:tt $(, $rest:tt)*) => {
        if $input == $idx {
            Box::new(amt::new::<$idx>($backend, $opts)) as Box<dyn AuthDB>
        } else {
            exaust_construct!($input, $backend, $opts, $($rest),*)
        }
    };
    ($input: ident, $backend: ident, $opts: ident, )=>{
        unreachable!("Unsupport index")
    }
}
use exaust_construct;
