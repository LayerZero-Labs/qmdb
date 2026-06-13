#![no_main]

use std::collections::{HashMap, HashSet};

use arbitrary::Arbitrary;
use libfuzzer_sys::fuzz_target;

use nomt::{
    hasher::Blake3Hasher, trie::KeyPath, KeyReadWrite, Nomt, Options, SessionParams, Value,
    WitnessMode,
};

fuzz_target!(|run: Run| {
    let db = open_db(run.commit_concurrency);

    for call in run.calls.calls {
        match call {
            NomtCall::BeginSession { session_calls } => {
                let session = db.begin_session(
                    SessionParams::default().witness_mode(WitnessMode::read_write()),
                );
                for session_call in session_calls {
                    match session_call {
                        SessionCall::TentativeRead {
                            key_path,
                            expected_value,
                        } => {
                            let actual_value = session.read(key_path).unwrap();
                            session.warm_up(key_path);
                            assert_eq!(actual_value, expected_value);
                        }
                        SessionCall::TentativeWrite { key_path } => {
                            session.warm_up(key_path);
                        }
                        SessionCall::CommitAndProve { keys } => {
                            let _ = session.finish(keys).unwrap().commit(&db).unwrap();
                            break;
                        }
                        SessionCall::Drop => {
                            drop(session);
                            break;
                        }
                    }
                }
            }
        }
    }
});

struct Context {
    /// All the key paths that have been created, irregardless of whether they have been committed
    /// or not.
    key_paths: HashSet<KeyPath>,
    /// The key value pairs that have been committed into the store.
    ///
    /// The u32 is the index of the key path in `key_paths`.
    committed: HashMap<KeyPath, Value>,
}

impl Context {
    fn new_key_path(
        &mut self,
        input: &mut arbitrary::Unstructured<'_>,
    ) -> arbitrary::Result<Option<KeyPath>> {
        let mut attempts = 0;
        loop {
            let mut key_path: [u8; 32] = [0; 32];
            input.fill_buffer(&mut key_path)?;
            if self.key_paths.insert(key_path) {
                return Ok(Some(key_path));
            }
            attempts += 1;
            if attempts > 10 {
                return Ok(None);
            }
        }
    }

    fn new_value(
        &mut self,
        input: &mut arbitrary::Unstructured<'_>,
    ) -> arbitrary::Result<Option<Value>> {
        if input.arbitrary()? {
            let mut size = *input.choose(&[0, 4096])?;
            if input.arbitrary()? {
                size += 1;
            }
            let mut buf = vec![0; size];
            input.fill_buffer(&mut buf)?;
            Ok(Some(buf.into()))
        } else {
            Ok(None)
        }
    }
}

#[derive(Debug)]
struct Run {
    commit_concurrency: usize,
    calls: NomtCalls,
}

impl<'a> Arbitrary<'a> for Run {
    fn arbitrary(input: &mut arbitrary::Unstructured<'a>) -> arbitrary::Result<Self> {
        let run = Run {
            commit_concurrency: input.int_in_range(1..=4)?,
            calls: input.arbitrary()?,
        };
        Ok(run)
    }
}

#[derive(Debug)]
enum NomtCall {
    BeginSession { session_calls: Vec<SessionCall> },
}

#[derive(Debug)]
enum SessionCall {
    TentativeRead {
        key_path: KeyPath,
        expected_value: Option<Value>,
    },
    TentativeWrite {
        key_path: KeyPath,
    },
    /// Commit and prove the given keys.
    CommitAndProve {
        keys: Vec<(KeyPath, KeyReadWrite)>,
    },
    /// Instructs to drop the session without committing.
    Drop,
}

#[derive(Debug)]
struct NomtCalls {
    pub calls: Vec<NomtCall>,
}

impl<'a> Arbitrary<'a> for NomtCalls {
    fn arbitrary(input: &mut arbitrary::Unstructured<'a>) -> arbitrary::Result<Self> {
        let call_cnt = input.int_in_range(0..=10)?;
        let mut calls = Vec::with_capacity(call_cnt);

        let mut cx = Context {
            key_paths: HashSet::new(),
            committed: HashMap::new(),
        };

        for _ in 0..call_cnt {
            let session_sz = input.int_in_range(0..=1000)?;

            // Collect all the available keys into a vector and destroy the non-determinism
            // by sorting them.
            let mut available_keys = cx.key_paths.iter().copied().collect::<Vec<_>>();
            available_keys.sort_unstable();

            let mut session: Vec<(KeyPath, KeyReadWrite)> = Vec::new();
            for _ in 0..session_sz {
                // Select a key: either an already used key or a brand new one.
                let key_path: KeyPath = if !available_keys.is_empty() && input.ratio(9, 10)? {
                    let idx = input.choose_index(available_keys.len())?;
                    available_keys.swap_remove(idx)
                } else {
                    // Create a brand new key.
                    match cx.new_key_path(input)? {
                        Some(key_path) => key_path,
                        None => {
                            // No new key path was created, so we need to skip this session.
                            break;
                        }
                    }
                };

                // Now, we need to choose if we want to read, write, or read-then-write with each
                // value.
                match *input.choose(&[0, 1, 2])? {
                    0 => {
                        // Check if the key has been committed, because we need our session ops be
                        // consistent with the committed state.
                        let value = cx.committed.get(&key_path).cloned();
                        session.push((key_path, KeyReadWrite::Read(value)));
                    }
                    1 => {
                        session.push((key_path, KeyReadWrite::Write(cx.new_value(input)?)));
                    }
                    2 => {
                        // Check if the key has been committed, because we need our session ops be
                        // consistent with the committed state.
                        let old_value = cx.committed.get(&key_path).cloned();
                        let new_value = cx.new_value(input)?;
                        session.push((key_path, KeyReadWrite::ReadThenWrite(old_value, new_value)));
                    }
                    _ => unreachable!(),
                }
            }

            session.sort_unstable_by(|(a, _), (b, _)| a.cmp(b));

            // Now having an idea what the session would look like we should create the calls to the
            // api.
            let mut session_calls = Vec::new();
            for (key_path, key_rw) in &session {
                match key_rw {
                    KeyReadWrite::ReadThenWrite(v, _) | KeyReadWrite::Read(v) => {
                        session_calls.push(SessionCall::TentativeRead {
                            key_path: *key_path,
                            expected_value: v.clone(),
                        });
                    }
                    KeyReadWrite::Write(_) => {
                        session_calls.push(SessionCall::TentativeWrite {
                            key_path: *key_path,
                        });
                    }
                }
            }

            let drop_session = input.ratio(1, 5)?;
            if drop_session {
                session_calls.push(SessionCall::Drop);
            } else {
                // Update committed state.
                for (key_path, key_rw) in &session {
                    match key_rw {
                        KeyReadWrite::ReadThenWrite(_, new_value)
                        | KeyReadWrite::Write(new_value) => {
                            if let Some(new_value) = new_value {
                                cx.committed.insert(*key_path, new_value.clone());
                            } else {
                                cx.committed.remove(key_path);
                            }
                        }
                        KeyReadWrite::Read(_) => {}
                    }
                }
                session_calls.push(SessionCall::CommitAndProve { keys: session });
            }

            calls.push(NomtCall::BeginSession { session_calls });
        }
        Ok(NomtCalls { calls })
    }
}

fn open_db(commit_concurrency: usize) -> Nomt<Blake3Hasher> {
    let tempfile = tempfile::tempdir().unwrap();
    let db_path = tempfile.path().join("db");
    let mut options = Options::new();
    options.path(db_path);
    options.commit_concurrency(commit_concurrency);
    Nomt::open(options).unwrap()
}
