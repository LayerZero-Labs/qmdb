use parking_lot::RwLock;
use qmdb::config::Config;
use qmdb::def::{IN_BLOCK_IDX_BITS, OP_CREATE, OP_DELETE, OP_WRITE};
use qmdb::tasks::TasksManager;
use qmdb::test_helper::SimpleTask;
use qmdb::utils::changeset::ChangeSet;
use qmdb::utils::{byte0_to_shard_id, hasher};
use qmdb::{AdsCore, AdsWrap, SharedAdsWrap, ADS};
use std::ffi::CStr;
use std::os::raw::{c_char, c_int};
use std::ptr;
use std::sync::Arc;

pub struct QmdbHandle {
    ads: Box<AdsWrap<SimpleTask>>,
}

pub struct QmdbSharedHandle {
    shared_ads: SharedAdsWrap,
}

pub struct QmdbChangeSet {
    changeset: ChangeSet,
}

pub struct QmdbTasksManager {
    tasks_manager: Arc<TasksManager<SimpleTask>>,
}

#[no_mangle]
pub extern "C" fn qmdb_init_dir(dir: *const c_char) -> c_int {
    if dir.is_null() {
        return -1;
    }

    let dir_str = unsafe {
        match CStr::from_ptr(dir).to_str() {
            Ok(s) => s,
            Err(_) => return -1,
        }
    };

    let config = Config::from_dir(dir_str);
    AdsCore::init_dir(&config);
    0
}

#[no_mangle]
pub extern "C" fn qmdb_new(dir: *const c_char) -> *mut QmdbHandle {
    if dir.is_null() {
        return ptr::null_mut();
    }

    let dir_str = unsafe {
        match CStr::from_ptr(dir).to_str() {
            Ok(s) => s,
            Err(_) => return ptr::null_mut(),
        }
    };

    let config = Config::from_dir(dir_str);
    let ads = AdsWrap::new(&config);

    Box::into_raw(Box::new(QmdbHandle { ads: Box::new(ads) }))
}

#[no_mangle]
pub extern "C" fn qmdb_free(handle: *mut QmdbHandle) {
    if !handle.is_null() {
        unsafe {
            let _ = Box::from_raw(handle);
        }
    }
}

#[no_mangle]
pub extern "C" fn qmdb_changeset_new() -> *mut QmdbChangeSet {
    Box::into_raw(Box::new(QmdbChangeSet {
        changeset: ChangeSet::new(),
    }))
}

#[no_mangle]
pub extern "C" fn qmdb_changeset_free(changeset: *mut QmdbChangeSet) {
    if !changeset.is_null() {
        unsafe {
            let _ = Box::from_raw(changeset);
        }
    }
}

#[no_mangle]
pub extern "C" fn qmdb_changeset_add_op(
    changeset: *mut QmdbChangeSet,
    op_type: u8,
    shard_id: u8,
    key_hash: *const u8,
    key_hash_len: usize,
    key: *const u8,
    key_len: usize,
    value: *const u8,
    value_len: usize,
) -> c_int {
    if changeset.is_null() || key_hash.is_null() {
        return -1;
    }

    let changeset = unsafe { &mut *changeset };
    let key_hash_slice = unsafe { std::slice::from_raw_parts(key_hash, key_hash_len) };
    let key_slice = if key.is_null() || key_len == 0 {
        &[]
    } else {
        unsafe { std::slice::from_raw_parts(key, key_len) }
    };
    let value_slice = if value.is_null() || value_len == 0 {
        &[]
    } else {
        unsafe { std::slice::from_raw_parts(value, value_len) }
    };

    if key_hash_len != 32 {
        return -1;
    }
    let mut key_hash_array = [0u8; 32];
    key_hash_array.copy_from_slice(key_hash_slice);
    changeset.changeset.add_op(
        op_type,
        shard_id,
        &key_hash_array,
        key_slice,
        value_slice,
        None,
    );
    0
}

#[no_mangle]
pub extern "C" fn qmdb_changeset_sort(changeset: *mut QmdbChangeSet) {
    if !changeset.is_null() {
        let changeset = unsafe { &mut *changeset };
        changeset.changeset.sort();
    }
}

#[no_mangle]
pub extern "C" fn qmdb_tasks_manager_new(
    changesets: *const *mut QmdbChangeSet,
    changeset_count: usize,
    last_task_id: i64,
) -> *mut QmdbTasksManager {
    if changesets.is_null() || changeset_count == 0 {
        return ptr::null_mut();
    }

    let mut task_list = Vec::with_capacity(changeset_count);
    for i in 0..changeset_count {
        let changeset_ptr = unsafe { *changesets.add(i) };
        if changeset_ptr.is_null() {
            return ptr::null_mut();
        }

        let changeset = unsafe { Box::from_raw(changeset_ptr) };
        let task = SimpleTask::new(vec![changeset.changeset]);
        task_list.push(RwLock::new(Some(task)));
    }

    Box::into_raw(Box::new(QmdbTasksManager {
        tasks_manager: Arc::new(TasksManager::new(task_list, last_task_id)),
    }))
}

#[no_mangle]
pub extern "C" fn qmdb_tasks_manager_free(manager: *mut QmdbTasksManager) {
    if !manager.is_null() {
        unsafe {
            let _ = Box::from_raw(manager);
        }
    }
}

#[no_mangle]
pub extern "C" fn qmdb_start_block(
    handle: *mut QmdbHandle,
    height: i64,
    tasks_manager: *mut QmdbTasksManager,
) -> c_int {
    if handle.is_null() || tasks_manager.is_null() {
        return -1;
    }

    let handle = unsafe { &mut *handle };
    let tasks_manager = unsafe { Box::from_raw(tasks_manager) };

    let (success, _meta_info) = handle.ads.start_block(height, tasks_manager.tasks_manager);
    if success {
        0
    } else {
        -1
    }
}

#[no_mangle]
pub extern "C" fn qmdb_get_shared(handle: *mut QmdbHandle) -> *mut QmdbSharedHandle {
    if handle.is_null() {
        return ptr::null_mut();
    }

    let handle = unsafe { &mut *handle };
    let shared_ads = handle.ads.get_shared();

    Box::into_raw(Box::new(QmdbSharedHandle { shared_ads }))
}

#[no_mangle]
pub extern "C" fn qmdb_shared_free(shared: *mut QmdbSharedHandle) {
    if !shared.is_null() {
        unsafe {
            let _ = Box::from_raw(shared);
        }
    }
}

#[no_mangle]
pub extern "C" fn qmdb_shared_insert_extra_data(
    shared: *mut QmdbSharedHandle,
    height: i64,
    data: *const c_char,
) -> c_int {
    if shared.is_null() || data.is_null() {
        return -1;
    }

    let shared = unsafe { &mut *shared };
    let data_str = unsafe {
        match CStr::from_ptr(data).to_str() {
            Ok(s) => s,
            Err(_) => return -1,
        }
    };

    shared
        .shared_ads
        .insert_extra_data(height, data_str.to_owned());
    0
}

#[no_mangle]
pub extern "C" fn qmdb_shared_add_task(shared: *mut QmdbSharedHandle, task_id: i64) {
    if !shared.is_null() {
        let shared = unsafe { &mut *shared };
        shared.shared_ads.add_task(task_id);
    }
}

#[no_mangle]
pub extern "C" fn qmdb_shared_read_entry(
    shared: *mut QmdbSharedHandle,
    height: i64,
    key_hash: *const u8,
    key_hash_len: usize,
    key: *const u8,
    key_len: usize,
    buf: *mut u8,
    buf_len: usize,
    found: *mut c_int,
) -> usize {
    if shared.is_null() || key_hash.is_null() || buf.is_null() || found.is_null() {
        return 0;
    }

    let shared = unsafe { &mut *shared };
    let key_hash_slice = unsafe { std::slice::from_raw_parts(key_hash, key_hash_len) };
    let key_slice = if key.is_null() || key_len == 0 {
        &[]
    } else {
        unsafe { std::slice::from_raw_parts(key, key_len) }
    };
    let buf_slice = unsafe { std::slice::from_raw_parts_mut(buf, buf_len) };

    let (size, found_entry) =
        shared
            .shared_ads
            .read_entry(height, key_hash_slice, key_slice, buf_slice);

    unsafe {
        *found = if found_entry { 1 } else { 0 };
    }

    size
}

#[no_mangle]
pub extern "C" fn qmdb_flush(handle: *mut QmdbHandle) -> c_int {
    if handle.is_null() {
        return -1;
    }

    let handle = unsafe { &mut *handle };
    let _meta_infos = handle.ads.flush();
    0
}

#[no_mangle]
pub extern "C" fn qmdb_hash(input: *const u8, input_len: usize, output: *mut u8) -> c_int {
    if input.is_null() || output.is_null() || input_len == 0 {
        return -1;
    }

    let input_slice = unsafe { std::slice::from_raw_parts(input, input_len) };
    let hash = hasher::hash(input_slice);

    unsafe {
        std::ptr::copy_nonoverlapping(hash.as_ptr(), output, 32);
    }

    0
}

#[no_mangle]
pub extern "C" fn qmdb_byte0_to_shard_id(byte0: u8) -> usize {
    byte0_to_shard_id(byte0)
}

#[no_mangle]
pub static QMDB_OP_CREATE: u8 = OP_CREATE;

#[no_mangle]
pub static QMDB_OP_WRITE: u8 = OP_WRITE;

#[no_mangle]
pub static QMDB_OP_DELETE: u8 = OP_DELETE;

#[no_mangle]
pub static QMDB_IN_BLOCK_IDX_BITS: u64 = IN_BLOCK_IDX_BITS as u64;
