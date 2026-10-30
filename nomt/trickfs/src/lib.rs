//! Trickfs — an experimental file system that is made for testing.
//!
//! The goal of this piece of code is a file system that can be used for
//! testing the behavior of other programs under certain conditions, such as:
//!
//! - Returning errors on certain operations. Prominently, `ENOSPC` — no space left on device.
//! - Simulating slow or fast operations.
//! - Returning corrupted data.
//! - Detecting reading not fsync-ed data, etc.
//!
//! Currently, this file system is implemented as in-memory, the storage is backed by `mmap`ed
//! memory. Only bare-bone operations are implemented, only those that are actually used by NOMT.

use std::{
    collections::BTreeMap,
    ffi::{OsStr, OsString},
    fmt,
    path::Path,
    sync::{atomic::AtomicBool, Arc, LazyLock, Mutex},
    time::{Duration, UNIX_EPOCH},
    u64,
};

mod latency;

use fuser::FileAttr;
use latency::LatencyInjector;

const DEFAULT_TTL: Duration = Duration::from_secs(1);
const BLK_SIZE: u64 = 512;
const MAX_FILE_SIZE: u64 = 1 << 40;

static DOT: LazyLock<OsString> = LazyLock::new(|| OsString::from("."));
static DOTDOT: LazyLock<OsString> = LazyLock::new(|| OsString::from(".."));

struct MmapStorage {
    ptr: *mut u8,
}

impl MmapStorage {
    pub fn new() -> MmapStorage {
        let ptr = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                MAX_FILE_SIZE as usize,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_PRIVATE | libc::MAP_ANONYMOUS | libc::MAP_NORESERVE,
                -1,
                0,
            )
        };
        if ptr == libc::MAP_FAILED {
            panic!("mmap failed");
        }
        MmapStorage { ptr: ptr.cast() }
    }

    // SAFETY: this is safe because we know that `ptr` points to a valid memory region of
    //         `MAX_FILE_SIZE` bytes. The lifetime and mutability of the slice is tied to the
    //         lifetime and mutability of `self`. u8 does not impose any alignment requirements.
    //
    //         One safety note here is that `ptr` is allocated with `MAP_NORESERVE` flag. That means
    //         that the memory in swap is not reserved for this allocation. It allows it to succeed
    //         with larger allocations than the physical memory available, both RAM and swap.
    //
    //         However, that means that in case the system runs out of memory and swap, the kernel
    //         will not be able to back the memory with physical memory and the process will be
    //         killed.
    //
    //         This is not considered a problem since a similiar thing can happen on any allocation
    //         path (e.g. `Vec::push`).

    /// Returns the underlying memory as a slice.
    pub fn as_slice(&self) -> &[u8] {
        // SAFETY: see the note above.
        unsafe { std::slice::from_raw_parts(self.ptr, MAX_FILE_SIZE as usize) }
    }

    /// Returns the underlying memory as a mutable slice.
    pub fn as_slice_mut(&mut self) -> &mut [u8] {
        // SAFETY: see the note above.
        unsafe { std::slice::from_raw_parts_mut(self.ptr, MAX_FILE_SIZE as usize) }
    }
}

impl Drop for MmapStorage {
    fn drop(&mut self) {
        unsafe {
            // SAFETY: we know that `ptr` points to a valid memory region of `MAX_FILE_SIZE` bytes
            //         previously allocated by `mmap`.
            libc::munmap(self.ptr.cast(), MAX_FILE_SIZE as usize);
        }
    }
}

// Safety: MmapStorage is just a chunk of memory, similiar to a Vec<_>, so it is safe to send
// across threads.
unsafe impl Send for MmapStorage {}

#[derive(Copy, Clone, Debug, Eq, PartialEq, Ord, PartialOrd)]
pub struct Inode(u64);

impl Inode {
    #[allow(dead_code)]
    const INVALID: Inode = Inode(0);
    const ROOT: Inode = Inode(1);
}

pub struct InodeData {
    parent: Inode,
    generation: u64,
    kind: fuser::FileType,
    file_size: u64,
    storage: Option<MmapStorage>,
}

impl InodeData {
    pub fn new_file(parent: Inode) -> Self {
        InodeData {
            parent,
            generation: 0,
            kind: fuser::FileType::RegularFile,
            file_size: 0,
            storage: None,
        }
    }

    pub fn new_dir(parent: Inode) -> Self {
        InodeData {
            parent,
            generation: 0,
            kind: fuser::FileType::Directory,
            file_size: 0,
            storage: None,
        }
    }

    /// Returns the inode containing this one.
    pub fn parent(&self) -> Inode {
        self.parent
    }

    /// Return the number of blocks in this file or directory.
    pub fn blocks(&self) -> u64 {
        // Derive the number of blocks from the file size.
        (self.file_size + BLK_SIZE - 1) / BLK_SIZE
    }

    /// Returns the size of the file in bytes.
    pub fn size(&self) -> u64 {
        self.file_size
    }

    /// Returns the kind of this inode.
    pub fn kind(&self) -> fuser::FileType {
        self.kind
    }

    pub fn is_dir(&self) -> bool {
        self.kind() == fuser::FileType::Directory
    }

    pub fn perm(&self) -> u16 {
        if self.kind() == fuser::FileType::Directory {
            0o755
        } else {
            0o644
        }
    }

    pub fn mk_file_attrs(&self, ino: Inode) -> FileAttr {
        FileAttr {
            ino: ino.0,
            size: self.size(),
            blocks: self.blocks(),
            atime: UNIX_EPOCH,
            mtime: UNIX_EPOCH,
            ctime: UNIX_EPOCH,
            crtime: UNIX_EPOCH,
            kind: self.kind(),
            perm: self.perm(),
            nlink: 1,
            uid: 0,
            gid: 0,
            blksize: BLK_SIZE as u32,
            rdev: 0,
            flags: 0,
        }
    }

    pub fn content(&self) -> &[u8] {
        if self.storage.is_none() {
            return &[];
        }
        self.storage.as_ref().unwrap().as_slice()
    }

    pub fn content_mut(&mut self) -> &mut [u8] {
        if self.storage.is_none() {
            self.storage = Some(MmapStorage::new());
        }
        self.storage.as_mut().unwrap().as_slice_mut()
    }

    fn set_size(&mut self, new_file_sz: u64) {
        self.file_size = new_file_sz;
    }
}

impl fmt::Debug for InodeData {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "InodeData {{ parent: {:?}, generation: {}, kind: {:?}, file_size: {} }}",
            self.parent, self.generation, self.kind, self.file_size
        )
    }
}

struct Container {
    names: Vec<OsString>,
    inos: Vec<Inode>,
}

impl Container {
    fn new() -> Self {
        Container {
            names: Vec::new(),
            inos: Vec::new(),
        }
    }

    fn lookup_by_name(&self, name: &OsStr) -> Option<Inode> {
        for (i, n) in self.names.iter().enumerate() {
            if n == name {
                return Some(self.inos[i]);
            }
        }
        None
    }

    fn lookup_by_inode(&self, ino: Inode) -> Option<&OsStr> {
        for (i, x) in self.inos.iter().enumerate() {
            if *x == ino {
                return Some(&self.names[i]);
            }
        }
        None
    }

    /// Removes the entry with the given name and removes its inode.
    fn remove(&mut self, name: &OsStr) -> Option<Inode> {
        for i in 0..self.names.len() {
            if self.names[i] == name {
                let ino = self.inos.remove(i);
                self.names.remove(i);
                return Some(ino);
            }
        }
        None
    }

    fn register(&mut self, name: OsString, ino: Inode) {
        self.names.push(name);
        self.inos.push(ino);
    }

    fn nth(&self, i: usize) -> Option<(&OsStr, Inode)> {
        if i >= self.names.len() {
            return None;
        }
        Some((&self.names[i], self.inos[i]))
    }

    fn count(&self) -> usize {
        self.names.len()
    }

    fn iter(&self) -> ReadDir<'_> {
        ReadDir {
            container: self,
            offset: 0,
        }
    }
}

struct ReadDir<'c> {
    container: &'c Container,
    offset: usize,
}

impl<'c> Iterator for ReadDir<'c> {
    type Item = (&'c OsStr, Inode);

    fn next(&mut self) -> Option<Self::Item> {
        if self.offset >= self.container.count() {
            return None;
        }
        let (name, ino) = self.container.nth(self.offset)?;
        self.offset += 1;
        Some((name, ino))
    }
}

struct Tree {
    ino_to_container: BTreeMap<Inode, Container>,
}

impl Tree {
    fn new() -> Self {
        Tree {
            ino_to_container: BTreeMap::new(),
        }
    }
}

/// The implementation of the file system.
pub struct Trick {
    tree: Tree,
    /// Stored inodes. The first inode is the root directory.
    ///
    /// Note that inodes are 1-based and this vector is 0-based.
    inodes: Vec<InodeData>,
    freelist: Vec<Inode>,
    trigger_enospc: Arc<AtomicBool>,
    trigger_latency_injector: Arc<AtomicBool>,
    latency_injector: LatencyInjector,
}

impl Trick {
    fn new(seed: u64) -> (Self, TrickHandle) {
        let trigger_enospc = Arc::new(AtomicBool::new(false));
        let trigger_latency_injector = Arc::new(AtomicBool::new(false));

        let tree = Tree::new();
        let inodes = Vec::new();
        let freelist = Vec::new();
        let mut fs = Trick {
            tree,
            inodes,
            freelist,
            trigger_enospc: trigger_enospc.clone(),
            trigger_latency_injector: trigger_latency_injector.clone(),
            latency_injector: LatencyInjector::new(seed),
        };
        // Initialize the root directory. Parent of the ROOT is ROOT.
        fs.register_inode(InodeData::new_dir(Inode::ROOT));
        fs.tree
            .ino_to_container
            .insert(Inode::ROOT, Container::new());

        let handle = TrickHandle {
            bg_sess: None.into(),
            trigger_enospc,
            trigger_latency_injector,
        };
        (fs, handle)
    }

    fn lookup_inode(&self, ino: Inode) -> Option<&InodeData> {
        let ino = ino.0;
        if ino == 0 {
            return None;
        }
        let inodes_index = usize::try_from(ino).unwrap() - 1;
        self.inodes.get(inodes_index)
    }

    fn lookup_inode_mut(&mut self, ino: Inode) -> Option<&mut InodeData> {
        let ino = ino.0;
        if ino == 0 {
            return None;
        }
        let inodes_index = usize::try_from(ino).unwrap() - 1;
        self.inodes.get_mut(inodes_index)
    }

    fn register_inode(&mut self, mut inode: InodeData) -> Inode {
        match self.freelist.pop() {
            Some(ino) => {
                let inodes_index = ino.0 as usize - 1;
                // Since we are reusing the inode we should bump its generation number.
                inode.generation = self.inodes[inodes_index].generation + 1;
                self.inodes[inodes_index] = inode;
                ino
            }
            None => {
                let ino = Inode(self.inodes.len() as u64 + 1);
                self.inodes.push(inode);
                ino
            }
        }
    }

    /// Marks the given inode as removed, prepares it for reusing.
    fn remove_inode(&mut self, removed_ino: Inode) {
        self.freelist.push(removed_ino);
    }

    fn reconstruct_full_path(&self, ino: Inode) -> OsString {
        let mut segments = Vec::<OsString>::new();
        let mut ino = ino;
        while ino != Inode::ROOT {
            let parent_inode = self.lookup_inode(ino).unwrap().parent();
            let container = self.tree.ino_to_container.get(&parent_inode).unwrap();
            let name = container.lookup_by_inode(ino).unwrap();
            segments.push(name.to_os_string());
            ino = parent_inode;
        }
        let mut path = OsString::new();
        path.push("/");
        for segment in segments.iter().rev() {
            path.push(segment);
            path.push("/");
        }
        path
    }

    /// Schedule the reply if `trigger_latency_injector` is on, otherwise reply directly.
    fn schedule_reply(&mut self, reply: impl FnOnce() + Send + 'static) {
        if !self
            .trigger_latency_injector
            .load(std::sync::atomic::Ordering::Relaxed)
        {
            reply();
        } else {
            self.latency_injector.schedule_reply(Box::new(reply));
        }
    }
}

impl fuser::Filesystem for Trick {
    fn lookup(
        &mut self,
        _req: &fuser::Request<'_>,
        parent: u64,
        name: &std::ffi::OsStr,
        reply: fuser::ReplyEntry,
    ) {
        log::trace!("lookup: parent={}, name={:?}", parent, name);
        let Some(parent) = self.tree.ino_to_container.get(&Inode(parent)) else {
            log::trace!("parent inode doesn't exist");
            reply.error(libc::ENOENT);
            return;
        };
        let Some(ino) = parent.lookup_by_name(name) else {
            log::trace!("file doesn't exist");
            reply.error(libc::ENOENT);
            return;
        };
        let Some(inode) = self.lookup_inode(ino) else {
            log::error!("inode doesn't exist. This looks like a bug.");
            reply.error(libc::ENOENT);
            return;
        };
        let file_attr = inode.mk_file_attrs(ino);
        let generation = inode.generation;
        self.schedule_reply(move || reply.entry(&DEFAULT_TTL, &file_attr, generation));
    }

    fn getattr(
        &mut self,
        _req: &fuser::Request<'_>,
        ino: u64,
        _fh: Option<u64>,
        reply: fuser::ReplyAttr,
    ) {
        let ino = Inode(ino);
        let Some(inode) = self.lookup_inode(ino) else {
            log::error!("inode doesn't exist. This looks like a bug.");
            reply.error(libc::ENOENT);
            return;
        };
        let file_attr = inode.mk_file_attrs(ino);
        self.schedule_reply(move || reply.attr(&DEFAULT_TTL, &file_attr));
    }

    fn setattr(
        &mut self,
        _req: &fuser::Request<'_>,
        ino: u64,
        mode: Option<u32>,
        uid: Option<u32>,
        gid: Option<u32>,
        size: Option<u64>,
        atime: Option<fuser::TimeOrNow>,
        mtime: Option<fuser::TimeOrNow>,
        ctime: Option<std::time::SystemTime>,
        fh: Option<u64>,
        crtime: Option<std::time::SystemTime>,
        chgtime: Option<std::time::SystemTime>,
        bkuptime: Option<std::time::SystemTime>,
        flags: Option<u32>,
        reply: fuser::ReplyAttr,
    ) {
        // trickfs doesn't track any times, so safely discard.
        let _ = (atime, mtime, ctime, crtime, chgtime, bkuptime);
        // discard those as well
        let _ = (mode, uid, gid, fh, flags);
        let ino = Inode(ino);
        let Some(inode) = self.lookup_inode_mut(ino) else {
            log::error!("inode doesn't exist. This looks like a bug.");
            reply.error(libc::ENOENT);
            return;
        };
        if let Some(new_size) = size {
            inode.set_size(new_size);
        }
        let file_attr = inode.mk_file_attrs(ino);
        self.schedule_reply(move || reply.attr(&DEFAULT_TTL, &file_attr));
    }

    fn create(
        &mut self,
        _req: &fuser::Request<'_>,
        parent: u64,
        name: &OsStr,
        mode: u32,
        umask: u32,
        flags: i32,
        reply: fuser::ReplyCreate,
    ) {
        // we don't really care about these parameters.
        let _ = (mode, umask, flags);
        if self
            .trigger_enospc
            .load(std::sync::atomic::Ordering::Relaxed)
        {
            reply.error(libc::ENOSPC);
            return;
        }
        let Some(parent_container) = self.tree.ino_to_container.get(&Inode(parent)) else {
            log::trace!("parent inode doesn't exist");
            reply.error(libc::ENOENT);
            return;
        };
        if parent_container.lookup_by_name(name).is_some() {
            log::trace!("file already exists");
            reply.error(libc::EEXIST);
            return;
        }
        let ino = self.register_inode(InodeData::new_file(Inode(parent)));
        // unwrap: we just checked that the parent exists.
        self.tree
            .ino_to_container
            .get_mut(&Inode(parent))
            .unwrap()
            .register(name.to_os_string(), ino);
        // unwrap: we just created this inode.
        let inode = self.lookup_inode(ino).unwrap();
        let file_attr = inode.mk_file_attrs(ino);
        let generation = inode.generation;
        self.schedule_reply(move || reply.created(&DEFAULT_TTL, &file_attr, generation, 0, 0));
    }

    fn open(&mut self, _req: &fuser::Request<'_>, ino: u64, _flags: i32, reply: fuser::ReplyOpen) {
        match self.lookup_inode(Inode(ino)) {
            Some(_inode_data) => self.schedule_reply(move || reply.opened(0, 0)),
            None => reply.error(libc::ENOENT),
        }
    }

    fn read(
        &mut self,
        _req: &fuser::Request<'_>,
        ino: u64,
        fh: u64,
        offset: i64,
        size: u32,
        flags: i32,
        lock_owner: Option<u64>,
        reply: fuser::ReplyData,
    ) {
        let _ = (fh, flags, lock_owner);
        let Some(inode_data) = self.lookup_inode(Inode(ino)) else {
            reply.error(libc::ENOENT);
            return;
        };
        log::trace!(
            "reading {:?} {:#?}",
            self.reconstruct_full_path(Inode(ino)),
            inode_data
        );
        // TODO: Check the offset.
        // TODO: O_DIRECT handling.
        //
        // If it is O_DIRECT we just need to serve the entire page.
        let size = size as usize;
        let Ok(offset) = usize::try_from(offset) else {
            reply.error(libc::EINVAL);
            return;
        };
        let end = offset + size;
        let content = &inode_data.content();
        if content.is_empty() {
            // The backing buffer has not yet been created. Let's just return an empty buffer.
            #[cfg(target_os = "linux")]
            let pageworth = has_odirect(flags);
            #[cfg(not(target_os = "linux"))]
            let pageworth = false;
            if pageworth {
                reply.data(&[0u8; 4096]);
            } else {
                reply.data(&[]);
            }
            return;
        }

        let content = if end > content.len() {
            &content[offset..]
        } else {
            &content[offset..end]
        };

        self.schedule_reply({
            let content = content.to_vec();
            move || {
                reply.data(&content);
            }
        });
    }

    fn write(
        &mut self,
        _req: &fuser::Request<'_>,
        ino: u64,
        fh: u64,
        offset: i64,
        data: &[u8],
        write_flags: u32,
        flags: i32,
        lock_owner: Option<u64>,
        reply: fuser::ReplyWrite,
    ) {
        let _ = (fh, flags, write_flags, lock_owner);
        let trigger_enospc = self
            .trigger_enospc
            .load(std::sync::atomic::Ordering::Relaxed);
        let Some(inode_data) = self.lookup_inode_mut(Inode(ino)) else {
            reply.error(libc::ENOENT);
            return;
        };
        let Ok(offset) = usize::try_from(offset) else {
            reply.error(libc::EINVAL);
            return;
        };
        let len = data.len();
        let new_file_sz = offset + len;
        if new_file_sz > MAX_FILE_SIZE as usize {
            reply.error(libc::EFBIG);
            return;
        }
        // Extension: if the file size is less than the new size, we should extend the file.
        if inode_data.size() < new_file_sz as u64 {
            if trigger_enospc {
                reply.error(libc::ENOSPC);
                return;
            }
            inode_data.set_size(new_file_sz as u64);
        }
        inode_data.content_mut()[offset..offset + len].copy_from_slice(data);
        self.schedule_reply(move || reply.written(len as u32));
    }

    fn mkdir(
        &mut self,
        _req: &fuser::Request<'_>,
        parent: u64,
        name: &OsStr,
        mode: u32,
        umask: u32,
        reply: fuser::ReplyEntry,
    ) {
        let _ = (mode, umask);
        if self
            .trigger_enospc
            .load(std::sync::atomic::Ordering::Relaxed)
        {
            reply.error(libc::ENOSPC);
            return;
        }
        let Some(inode_data) = self.lookup_inode_mut(Inode(parent)) else {
            reply.error(libc::ENOENT);
            return;
        };
        if !inode_data.is_dir() {
            reply.error(libc::ENOTDIR);
            return;
        }
        let ino = self.register_inode(InodeData::new_dir(Inode(parent)));
        self.tree.ino_to_container.insert(ino, Container::new());
        self.tree
            .ino_to_container
            .get_mut(&Inode(parent))
            .unwrap()
            .register(name.to_os_string(), ino);
        // unwrap: we just created this inode.
        let inode = self.lookup_inode(ino).unwrap();
        let file_attr = inode.mk_file_attrs(ino);
        let generation = inode.generation;
        self.schedule_reply(move || reply.entry(&DEFAULT_TTL, &file_attr, generation));
    }

    fn readdir(
        &mut self,
        _req: &fuser::Request<'_>,
        ino: u64,
        fh: u64,
        offset: i64,
        mut reply: fuser::ReplyDirectory,
    ) {
        let _ = fh;
        let Some(inode_data) = self.lookup_inode_mut(Inode(ino)) else {
            reply.error(libc::ENOENT);
            return;
        };
        if !inode_data.is_dir() {
            reply.error(libc::ENOTDIR);
            return;
        }
        let parent = inode_data.parent();
        let container = self
            .tree
            .ino_to_container
            .get(&Inode(ino))
            .unwrap_or_else(|| panic!("ino {ino} is not in tree"));
        let offset = u64::try_from(offset).unwrap();
        let standard = &[(DOT.as_os_str(), Inode(ino)), (DOTDOT.as_os_str(), parent)];
        let mut iter = standard
            .iter()
            .copied()
            .chain(container.iter())
            .enumerate()
            .skip(offset as usize);
        loop {
            if let Some((offset, (name, ino))) = iter.next() {
                let inode = self
                    .lookup_inode(ino)
                    .unwrap_or_else(|| panic!("{ino:?} cannot be found"));
                let offset = offset + 1;
                let buf_is_filled =
                    reply.add(ino.0, offset.try_into().unwrap(), inode.kind(), name);
                if buf_is_filled {
                    break;
                }
            } else {
                break;
            }
        }
        self.schedule_reply(move || reply.ok());
    }

    fn rmdir(
        &mut self,
        _req: &fuser::Request<'_>,
        parent: u64,
        name: &OsStr,
        reply: fuser::ReplyEmpty,
    ) {
        let Some(container) = self.tree.ino_to_container.get(&Inode(parent)) else {
            reply.error(libc::ENOENT);
            return;
        };
        let Some(inode) = container.lookup_by_name(name) else {
            reply.error(libc::ENOENT);
            return;
        };
        let Some(inode_data) = self.lookup_inode(inode) else {
            reply.error(libc::ENOENT);
            return;
        };
        if !inode_data.is_dir() {
            reply.error(libc::ENOTDIR);
            return;
        }
        // unwrap: we checked that the dir exists above.
        let container = self.tree.ino_to_container.get_mut(&Inode(parent)).unwrap();
        let Some(removed_ino) = container.remove(name) else {
            reply.error(libc::ENOENT);
            return;
        };
        if container.count() > 0 {
            // Note that VFS should not allow removing a non-empty directory and so
            // we do not expect here to encounter one.
            //
            // When you do `rm -rf` on a directory, the kernel will first remove all the
            // entries in the directory and then remove the directory itself. So we should
            // not encounter a non-empty directory here.
            return reply.error(libc::ENOTEMPTY);
        }
        // Remove the tree entry corresponding to the removed inode.
        self.tree
            .ino_to_container
            .remove(&removed_ino)
            .unwrap_or_else(|| panic!("container was not present"));
        self.remove_inode(removed_ino);
        self.schedule_reply(move || reply.ok());
    }

    fn unlink(
        &mut self,
        _req: &fuser::Request<'_>,
        parent: u64,
        name: &OsStr,
        reply: fuser::ReplyEmpty,
    ) {
        let Some(inode_data) = self.lookup_inode_mut(Inode(parent)) else {
            reply.error(libc::ENOENT);
            return;
        };
        if !inode_data.is_dir() {
            reply.error(libc::ENOTDIR);
            return;
        }
        let Some(container) = self.tree.ino_to_container.get_mut(&Inode(parent)) else {
            reply.error(libc::ENOENT);
            return;
        };
        let Some(removed_ino) = container.remove(name) else {
            reply.error(libc::ENOENT);
            return;
        };
        self.remove_inode(removed_ino);
        self.schedule_reply(move || reply.ok());
    }

    fn fallocate(
        &mut self,
        _req: &fuser::Request<'_>,
        ino: u64,
        fh: u64,
        offset: i64,
        length: i64,
        mode: i32,
        reply: fuser::ReplyEmpty,
    ) {
        let _ = (fh, mode);
        let trigger_enospc = self
            .trigger_enospc
            .load(std::sync::atomic::Ordering::Relaxed);
        let Some(inode_data) = self.lookup_inode_mut(Inode(ino)) else {
            reply.error(libc::ENOENT);
            return;
        };
        // fallocate should preallocate stuff. We here just gon pretend that we are preallocating.
        // Here we should extend the file if the offset + length is greater than the current file.
        let new_size = u64::try_from(offset).unwrap() + u64::try_from(length).unwrap();
        if inode_data.size() < new_size {
            if trigger_enospc {
                reply.error(libc::ENOSPC);
                return;
            }
            inode_data.set_size(new_size);
        }
        self.schedule_reply(move || reply.ok());
    }

    fn fsync(
        &mut self,
        _req: &fuser::Request<'_>,
        ino: u64,
        fh: u64,
        datasync: bool,
        reply: fuser::ReplyEmpty,
    ) {
        // fsync doesn't do anything since we are working in-memory, so just return OK.
        let _ = (ino, fh, datasync);
        if self
            .trigger_enospc
            .load(std::sync::atomic::Ordering::Relaxed)
        {
            reply.error(libc::ENOSPC);
            return;
        }
        self.schedule_reply(move || reply.ok());
    }

    fn fsyncdir(
        &mut self,
        req: &fuser::Request<'_>,
        ino: u64,
        fh: u64,
        datasync: bool,
        reply: fuser::ReplyEmpty,
    ) {
        // just like fsync, fsyncdir doesn't do anything.
        let _ = (req, ino, fh, datasync);
        if self
            .trigger_enospc
            .load(std::sync::atomic::Ordering::Relaxed)
        {
            reply.error(libc::ENOSPC);
            return;
        }
        self.schedule_reply(move || reply.ok());
    }
}

#[cfg(target_os = "linux")]
fn has_odirect(flags: i32) -> bool {
    (flags & libc::O_DIRECT) != 0
}

pub struct TrickHandle {
    bg_sess: Mutex<Option<fuser::BackgroundSession>>,
    trigger_enospc: Arc<AtomicBool>,
    trigger_latency_injector: Arc<AtomicBool>,
}

impl TrickHandle {
    /// Sets whether the file system should return ENOSPC on the subsequent write operations.
    pub fn set_trigger_enospc(&self, on: bool) {
        self.trigger_enospc
            .store(on, std::sync::atomic::Ordering::Relaxed);
    }

    /// Sets whether the file system should inject latencies on every operation.
    pub fn set_trigger_latency_injector(&self, on: bool) {
        self.trigger_latency_injector
            .store(on, std::sync::atomic::Ordering::Relaxed);
    }

    pub fn unmount_and_join(self) {
        if let Some(bg_sess) = self.bg_sess.lock().unwrap().take() {
            bg_sess.join();
        }
    }
}

/// A convenience function to spawn the trick file system.
///
/// This allows directly depending on the libfuse API.
pub fn spawn_trick<P: AsRef<Path>>(mountpoint: P, seed: u64) -> std::io::Result<TrickHandle> {
    use fuser::MountOption;
    let options = &[
        MountOption::RW,
        MountOption::AutoUnmount,
        MountOption::FSName("trick".to_string()),
    ];
    let (fs, mut handle) = Trick::new(seed);
    handle.bg_sess = Some(fuser::spawn_mount2(fs, &mountpoint, options)?).into();
    Ok(handle)
}

#[cfg(test)]
mod tests {
    use super::Trick;
    use fuser::MountOption;
    use std::{
        fs,
        io::{Read, Seek, Write as _},
        os::fd::AsRawFd,
    };

    fn init_log() {
        let _ = env_logger::builder()
            .filter_level(log::LevelFilter::Trace)
            .is_test(true)
            .try_init();
    }

    // Just a smoke test to make sure the file system can be mounted and unmounted.
    //
    // If this fails then something is terribly wrong.
    #[test]
    fn mount() {
        init_log();
        let mountpoint = tempfile::tempdir().unwrap();
        let options = &[MountOption::RO, MountOption::FSName("trick".to_string())];
        let (fs, _handle) = Trick::new(0);
        let mount_handle = fuser::spawn_mount2(fs, &mountpoint, options).unwrap();
        drop(mount_handle);
    }

    // Create a file to the file system.
    #[test]
    fn create_file() {
        init_log();
        let mountpoint = tempfile::tempdir().unwrap();
        let options = &[MountOption::RW, MountOption::FSName("trick".to_string())];
        let (fs, _handle) = Trick::new(0);
        let mount_handle = fuser::spawn_mount2(fs, &mountpoint, options).unwrap();
        let filename = mountpoint.path().join("file");
        let file = fs::File::create(&filename).unwrap();
        drop(file);
        drop(mount_handle);
    }

    #[test]
    fn create_then_open_file() {
        init_log();
        let mountpoint = tempfile::tempdir().unwrap();
        let options = &[MountOption::RW, MountOption::FSName("trick".to_string())];
        let (fs, _handle) = Trick::new(0);
        let mount_handle = fuser::spawn_mount2(fs, &mountpoint, options).unwrap();
        let filename = mountpoint.path().join("file");
        let file = fs::File::create(&filename).unwrap();
        drop(file);
        let file = fs::File::open(&filename).unwrap();
        drop(file);
        drop(mount_handle);
    }

    fn inner_write_then_read(fs: Trick) {
        init_log();
        let mountpoint = tempfile::tempdir().unwrap();
        let options = &[
            MountOption::RW,
            MountOption::AutoUnmount,
            MountOption::FSName("trick".to_string()),
        ];
        let mount_handle = fuser::spawn_mount2(fs, &mountpoint, options).unwrap();
        let filename = mountpoint.path().join("file");
        let mut file = fs::File::options()
            .create_new(true)
            .write(true)
            .open(&filename)
            .unwrap();
        let test_data = b"hello world";
        file.write_all(test_data).unwrap();
        drop(file);
        // reopen and read
        let mut file = fs::File::open(&filename).unwrap();
        let mut buf = Vec::new();
        file.read_to_end(&mut buf).unwrap();
        assert_eq!(test_data, buf.as_slice());
        drop(file);
        drop(mount_handle);
    }

    #[test]
    fn write_then_read() {
        let (fs, _handle) = Trick::new(0);
        inner_write_then_read(fs);
    }

    #[test]
    fn write_then_read_with_latency() {
        let (fs, handle) = Trick::new(0);
        handle.set_trigger_latency_injector(true);
        inner_write_then_read(fs);
    }

    // Create many files and write to them at increasing offsets.
    //
    // This is supposed to test that we can handle many sparse files and that it does not run out
    // of memory.
    #[test]
    fn many_files() {
        init_log();
        let mountpoint = tempfile::tempdir().unwrap();
        let options = &[
            MountOption::RW,
            MountOption::AutoUnmount,
            MountOption::FSName("trick".to_string()),
        ];
        let (fs, _handle) = Trick::new(0);
        let mount_handle = fuser::spawn_mount2(fs, &mountpoint, options).unwrap();
        let mut files = Vec::new();
        for i in 0..100 {
            let filename = mountpoint.path().join(format!("file{}", i));
            let mut file = fs::File::options()
                .create_new(true)
                .write(true)
                .open(&filename)
                .unwrap();
            let test_data = format!("hello world {}", i);
            file.seek_relative(i * 10_000).unwrap();
            file.write_all(test_data.as_bytes()).unwrap();
            files.push(file);
        }
        for file in files {
            drop(file);
        }
        drop(mount_handle);
    }

    /// Create and remove a file.
    #[test]
    fn unlink() {
        init_log();
        let mountpoint = tempfile::tempdir().unwrap();
        let options = &[
            MountOption::RW,
            MountOption::AutoUnmount,
            MountOption::FSName("trick".to_string()),
        ];
        let (fs, _handle) = Trick::new(0);
        let mount_handle = fuser::spawn_mount2(fs, &mountpoint, options).unwrap();

        // Create a file
        let filename = mountpoint.path().join("file_to_unlink");
        let file = fs::File::create(&filename).unwrap();
        drop(file);
        assert!(filename.exists());
        fs::remove_file(&filename).unwrap();
        assert!(!filename.exists());
        drop(mount_handle);
    }

    #[test]
    fn mmap_test() {
        init_log();
        let mountpoint = tempfile::tempdir().unwrap();
        let options = &[
            MountOption::RW,
            MountOption::AutoUnmount,
            MountOption::FSName("trick".to_string()),
        ];
        let (fs, _handle) = Trick::new(0);
        let mount_handle = fuser::spawn_mount2(fs, &mountpoint, options).unwrap();

        let filename = mountpoint.path().join("file");
        let mut file = fs::File::options()
            .create_new(true)
            .write(true)
            .open(&filename)
            .unwrap();
        let test_data = b"hello world";
        file.write_all(test_data).unwrap();
        drop(file);

        let file = fs::File::open(&filename).unwrap();
        unsafe {
            let data = libc::mmap(
                std::ptr::null_mut(),
                4096,
                libc::PROT_READ,
                libc::MAP_PRIVATE,
                file.as_raw_fd(),
                0,
            );
            assert_ne!(data, libc::MAP_FAILED);
            let data: *mut u8 = data.cast();
            let slice = std::slice::from_raw_parts(data, 4096);
            assert_eq!(&test_data[..], &slice[0..test_data.len()][..]);
        }

        drop(file);
        drop(mount_handle);
    }

    #[test]
    fn out_of_space() {
        init_log();
        let mountpoint = tempfile::tempdir().unwrap();
        let options = &[
            MountOption::RW,
            MountOption::AutoUnmount,
            MountOption::FSName("trick".to_string()),
        ];
        let (fs, handle) = Trick::new(0);
        let mount_handle = fuser::spawn_mount2(fs, &mountpoint, options).unwrap();

        let filename = mountpoint.path().join("file");
        let mut file = fs::File::options()
            .create_new(true)
            .write(true)
            .open(&filename)
            .unwrap();

        let test_data = b"hello world";
        handle.set_trigger_enospc(true);
        let _ = file.write_all(test_data).unwrap_err();
        handle.set_trigger_enospc(false);
        let _ = file.write_all(test_data).unwrap();

        drop(file);
        drop(mount_handle);
    }
}
