use pprof::{protos::Message, ProfilerGuard, Report};
use std::fs::File;
use std::io::Write;

pub struct Profiler {
    inner: Option<ProfilerGuard<'static>>,
    frequency: i32,
    reports: Vec<Report>,
}

impl Profiler {
    pub fn new(frequency: i32) -> Self {
        if frequency > 0 {
            Self {
                inner: Some(pprof::ProfilerGuard::new(frequency).unwrap()),
                frequency,
                reports: Vec::new(),
            }
        } else {
            Self {
                inner: None,
                frequency,
                reports: Vec::new(),
            }
        }
    }

    pub fn tick(&mut self) {
        if self.frequency <= 0 {
            return;
        }
        let profiler = std::mem::take(&mut self.inner).unwrap();
        let report = profiler.report().build().unwrap();
        self.reports.push(report);
        std::mem::drop(profiler);
        self.inner = Some(pprof::ProfilerGuard::new(self.frequency).unwrap())
    }

    pub fn report_to_file(self, prefix: &str) {
        if self.frequency <= 0 {
            return;
        }
        print!("Writing profiles... ");

        for (index, report) in self.reports.into_iter().enumerate() {
            let path = format!("{}_{:02}.pb", prefix, index);
            let mut file = File::create(path).unwrap();
            let profile = report.pprof().unwrap();

            let mut content = Vec::new();
            profile.encode(&mut content).unwrap();
            file.write_all(&content).unwrap();
        }
        println!("Done");
    }
}
