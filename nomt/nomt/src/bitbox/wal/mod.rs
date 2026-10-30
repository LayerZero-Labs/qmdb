const WAL_ENTRY_TAG_START: u8 = 1;
const WAL_ENTRY_TAG_END: u8 = 2;
const WAL_ENTRY_TAG_CLEAR: u8 = 3;
const WAL_ENTRY_TAG_UPDATE: u8 = 4;

pub use read::{WalBlobReader, WalEntry};
pub use write::WalBlobBuilder;

mod read;
mod write;

#[cfg(test)]
mod tests;
