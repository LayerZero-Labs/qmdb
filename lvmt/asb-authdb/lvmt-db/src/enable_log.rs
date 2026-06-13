#![allow(dead_code)]

use log::LevelFilter;
use log4rs::{
    append::console::ConsoleAppender,
    config::{Appender, Config as LogConfig, Logger, Root},
};

pub fn enable_log(level: LevelFilter) -> Result<(), String> {
    let mut conf_builder = LogConfig::builder().appender(
        Appender::builder().build("stdout", Box::new(ConsoleAppender::builder().build())),
    );
    let root_builder = Root::builder().appender("stdout");
    // Should add new crate names here
    for crate_name in ["lvmt-db", "amt-bench"].iter() {
        conf_builder = conf_builder.logger(Logger::builder().build(*crate_name, level));
    }
    let log_config = conf_builder
        .build(root_builder.build(level))
        .map_err(|e| format!("failed to build log config: {:?}", e))?;
    log4rs::init_config(log_config)
        .map_err(|e| format!("failed to initialize log with config: {:?}", e))?;
    Ok(())
}

pub fn enable_debug_log() {
    enable_log(LevelFilter::Debug).unwrap();
}

pub fn enable_info_log() {
    enable_log(LevelFilter::Info).unwrap();
}
