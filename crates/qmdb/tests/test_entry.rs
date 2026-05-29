use qmdb_common::def::{ENTRY_BASE_LENGTH, NULL_ENTRY_VERSION, SHARD_COUNT};
use qmdb_common::entryfile::entry;
use qmdb_common::utils::hasher::{blake2b_hash_inplace, blake3_hash_inplace};

#[test]
fn test_entry() {
    let len_bytes = [7, 6, 5, 4];
    let (key_len, value_len) = entry::EntryBz::get_kv_len(&len_bytes);
    assert_eq!(0x7, key_len);
    assert_eq!(0x040506, value_len);

    let mut sentry_entry_buf = [0u8; ENTRY_BASE_LENGTH + 32 + 8];
    let sentry_entry = entry::sentry_entry(SHARD_COUNT / 8, 5, &mut sentry_entry_buf);
    assert_eq!(5, sentry_entry.serial_number());
    let mut hash: [u8; 32] = [0; 32];
    hash[0] = 0x20;
    hash[1] = 0x05;
    let mut next_hash = [0x0; 32];
    next_hash[0] = 0x20;
    next_hash[1] = 0x06;
    assert_eq!(hash.as_slice(), sentry_entry.key());
    assert_eq!(hash, sentry_entry.key_hash());
    assert_eq!(next_hash.as_slice(), sentry_entry.next_key_hash());
    assert_eq!(0, sentry_entry.value().len());
    #[cfg(not(feature = "tee_cipher"))]
    let size = sentry_entry.bz.len() - 3; //exclude padding bytes
    #[cfg(feature = "tee_cipher")]
    let size = sentry_entry.bz.len() - qmdb::def::TAG_SIZE - 3; //exclude padding bytes
    let mut x = [0u8; 32];
    blake2b_hash_inplace(&mut x, &sentry_entry.bz[..size]);
    assert_eq!(x, sentry_entry.hash());
    assert_eq!(0, sentry_entry.version());
    assert_eq!(5, sentry_entry.serial_number());
    assert_eq!(0, sentry_entry.dsn_count());

    let zero32 = [0u8; 32];
    let mut null_entry_buf = [0; ENTRY_BASE_LENGTH + 32 + 8];
    let null_entry = entry::null_entry(&mut null_entry_buf);
    assert_eq!(0, null_entry.key().len());
    let x = [0u8; 32];
    assert_eq!(x, null_entry.key_hash());
    assert_eq!(zero32.as_slice(), null_entry.next_key_hash());
    assert_eq!(0, null_entry.value().len());
    #[cfg(not(feature = "tee_cipher"))]
    let size = null_entry.bz.len() - 3; //exclude padding bytes
    #[cfg(feature = "tee_cipher")]
    let size = null_entry.bz.len() - qmdb::def::TAG_SIZE - 3; //exclude padding bytes
    let mut x = [0u8; 32];
    blake2b_hash_inplace(&mut x, &null_entry.bz[..size]);
    assert_eq!(x, null_entry.hash());
    assert_eq!(NULL_ENTRY_VERSION, null_entry.version());
    assert_eq!(u64::MAX, null_entry.serial_number());
    assert_eq!(0, null_entry.dsn_count());

    let mut next_key_hash = [0u8; 32];
    blake3_hash_inplace(&mut next_key_hash, String::from("nextkey").as_bytes());
    let x: [u8; 32] = next_key_hash;
    let key = String::from("key");
    let value = String::from("value");
    let entry = entry::Entry {
        key: key.as_bytes(),
        value: value.as_bytes(),
        next_key_hash: &x,
        version: 10000,
        serial_number: 800,
    };
    let mut b = vec![0; 200];
    let deactived_serial_num_list = vec![0, 1, 2, 3];
    let entry_bz_size = entry.dump(&mut b, &deactived_serial_num_list);
    let entry_bz = entry::EntryBz {
        bz: &b[..entry_bz_size],
    };
    assert_eq!(entry.key, entry_bz.key());
    let mut x = [0u8; 32];
    blake3_hash_inplace(&mut x, entry.key);
    assert_eq!(x, entry_bz.key_hash());
    assert_eq!(entry.next_key_hash, entry_bz.next_key_hash());
    assert_eq!(entry.value, entry_bz.value());
    assert_eq!(entry.version, entry_bz.version());
    assert_eq!(entry.serial_number, entry_bz.serial_number());
    assert_eq!(deactived_serial_num_list.len(), entry_bz.dsn_count());
    for (i, &deactived_sn) in deactived_serial_num_list.iter().enumerate() {
        assert_eq!(deactived_sn, entry_bz.get_deactived_sn(i));
    }
}
