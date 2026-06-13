#![allow(unused)]
use std::{
    collections::VecDeque, fs, path::Path, sync::mpsc::sync_channel, thread::Thread, time::Duration,
};

use postcard::from_bytes;
use serde::{Deserialize, Serialize};
use std::sync::mpsc::{channel, Receiver, Sender};

use asb_options::{AuthAlgo, Options};

use super::{Event, Events, TaskTrait};

#[derive(Clone, Debug, Serialize, Deserialize)]
enum ExperimentTask {
    Read([u8; 32]),
    Write([u8; 32], Bytes),
}

type Bytes = Vec<u8>;
type InitTasks = Vec<([u8; 32], Bytes)>;
type BlockTask = VecDeque<ExperimentTask>;

fn read_from_file<T, S: AsRef<Path>>(path: S) -> T
where
    for<'a> T: Deserialize<'a>,
{
    let loaded = std::fs::read(path.as_ref()).unwrap();
    from_bytes(&loaded).unwrap()
}

pub struct TaskProducer {
    receiver: Receiver<Vec<Events>>,
    group_size: usize,
    events: Vec<Events>,
}

impl TaskProducer {
    fn new(path: String, group_size: usize) -> Self {
        let (sender, receiver) = sync_channel(1);
        std::thread::spawn(move || {
            let path = Path::new(&path);
            let mut idx = 0usize;
            let mut next_file = path.join(format!("real_trace.{}.data", idx));
            while fs::metadata(&next_file).is_ok() {
                let loaded = std::fs::read(next_file).unwrap();
                let block_group: Vec<Vec<ExperimentTask>> = from_bytes(&loaded).unwrap();
                let events = block_group
                    .into_iter()
                    .map(|block| {
                        Events(
                            block
                                .into_iter()
                                .map(|io| match io {
                                    ExperimentTask::Read(key) => Event::Read(key.to_vec()),
                                    ExperimentTask::Write(key, value) => {
                                        Event::Write(key.to_vec(), value.clone())
                                    }
                                })
                                .collect::<Vec<_>>(),
                        )
                    })
                    .collect();
                sender.send(events);

                idx += 1;
                next_file = path.join(format!("real_trace.{}.data", idx));
            }
        });
        Self {
            receiver,
            events: vec![],
            group_size,
        }
    }

    fn pick_next(&mut self) -> Option<Events> {
        if self.events.is_empty() {
            match self.receiver.recv_timeout(Duration::from_secs(1)) {
                Ok(events) => {
                    self.events = events;
                }
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                    panic!("Load data timeout");
                }
                Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => return None,
            }
        }

        Some(self.events.pop().unwrap())
    }
}

impl Iterator for TaskProducer {
    type Item = Events;
    fn next(&mut self) -> Option<Events> {
        if self.group_size == 1 {
            self.pick_next()
        } else {
            let mut grouped_events = Vec::with_capacity(100_000);
            for i in 0..self.group_size {
                if let Some(events) = self.pick_next() {
                    grouped_events.extend(events.0);
                }
            }
            if grouped_events.is_empty() {
                None
            } else {
                Some(Events(grouped_events))
            }
        }
    }
}

pub struct RealTrace {
    path: String,
    init_tasks: Option<InitTasks>,
    group_size: usize,
}

impl RealTrace {
    pub fn new(opt: &Options, load_warmup: bool) -> Self {
        RealTrace {
            path: opt.trace_dir.clone(),
            init_tasks: if load_warmup {
                Some(read_from_file(
                    &Path::new(&opt.trace_dir).join(format!("real_trace.init")),
                ))
            } else {
                None
            },
            group_size: if opt.algorithm == AuthAlgo::RAIN || opt.algorithm == AuthAlgo::MPT {
                50
            } else {
                1
            },
        }
    }
}

impl TaskTrait for RealTrace {
    fn tasks(&self) -> Box<dyn Iterator<Item = Events>> {
        Box::new(TaskProducer::new(self.path.clone(), self.group_size))
    }

    fn warmup<'a>(&'a self) -> Box<dyn Iterator<Item = Events> + 'a> {
        Box::new(self.init_tasks.as_ref().unwrap().chunks(1000).map(|arr| {
            Events(
                arr.iter()
                    .map(|(key, value)| Event::Write(key.to_vec(), value.clone()))
                    .collect::<Vec<_>>(),
            )
        }))
    }
}
