use std::sync::atomic::{AtomicUsize, Ordering};
use lazy_static::lazy_static;
use crate::def::SHARD_COUNT;

lazy_static! {
    static ref CURRENT_SHARD_COUNT: AtomicUsize = AtomicUsize::new(SHARD_COUNT);
}

pub fn set_shard_count(count: usize) {
    // Ensure shard count is a power of 2 and at least 1
    let count = count.next_power_of_two().max(1);
    CURRENT_SHARD_COUNT.store(count, Ordering::SeqCst);
}

pub fn get_current_shard_count() -> usize {
    CURRENT_SHARD_COUNT.load(Ordering::SeqCst)
}

pub fn get_current_sentry_count() -> usize {
    (1 << 16) / get_current_shard_count()
}

pub fn get_current_shard_div() -> usize {
    (1 << 16) / get_current_shard_count()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::def::{SENTRY_COUNT, SHARD_DIV};

    #[test]
    fn test_shard_count_power_of_two() {
        set_shard_count(3);
        assert_eq!(get_current_shard_count(), 4);
        
        set_shard_count(15);
        assert_eq!(get_current_shard_count(), 16);
        
        set_shard_count(17);
        assert_eq!(get_current_shard_count(), 32);
    }

    #[test]
    fn test_sentry_count() {
        set_shard_count(16);
        assert_eq!(get_current_sentry_count(), SENTRY_COUNT);
        
        set_shard_count(32);
        assert_eq!(get_current_sentry_count(), SENTRY_COUNT / 2);
    }

    #[test]
    fn test_shard_div() {
        set_shard_count(16);
        assert_eq!(get_current_shard_div(), SHARD_DIV);
        
        set_shard_count(32);
        assert_eq!(get_current_shard_div(), SHARD_DIV / 2);
    }
} 