use super::PAGE_SIZE;
use parking_lot::{RwLock, RwLockWriteGuard};
use std::{
    cell::RefCell,
    ops::{Deref, DerefMut},
    sync::{
        atomic::{AtomicPtr, AtomicU32, Ordering},
        Arc,
    },
};
use thread_local::ThreadLocal;

// Region is 256 MiB. The choice is mostly arbitrary, but:
//
// 1. it's big enough so that we don't have to allocate too often.
// 2. it's a multiple of 2 MiB which is the size of huge-page on x86-64 and aarch64.
// 3. it fits 65k 4 KiB pages which requires 2 bytes to address making it nice round number.
const REGION_SLOT_BITS: u32 = 16;
const SLOTS_PER_REGION: usize = 1 << REGION_SLOT_BITS;
const REGION_BYTE_SIZE: usize = SLOTS_PER_REGION * PAGE_SIZE;
const REGION_COUNT: usize = 4096;

const TLS_FREELIST_CAPACITY: usize = 1024;

/// A page reference to the pool.
#[derive(Clone)]
pub struct Page(*mut u8);

unsafe impl Send for Page {}
unsafe impl Sync for Page {}

impl Page {
    /// Returns a pointer to the page.
    pub fn as_ptr(&self) -> *const u8 {
        self.0
    }

    /// Returns a mutable pointer to the page.
    pub fn as_mut_ptr(&self) -> *mut u8 {
        self.0
    }

    /// This is a convenience function that uses [`std::slice::from_raw_parts_mut`] to create a
    /// mutable slice.
    ///
    /// # Safety
    ///
    /// The caller is responsible for making sure:
    ///
    /// 1. that the page is not freed,
    /// 2. that the [`PagePool`] is the same that was used to allocate the page.
    /// 3. that the [`PagePool`] is not dropped while the slice is used.
    /// 4. that there is only a single mutable slice into the page at any given time.
    pub unsafe fn as_mut_slice(&self) -> &mut [u8] {
        std::slice::from_raw_parts_mut(self.as_mut_ptr(), PAGE_SIZE)
    }
}

/// Provides a managed version of a [`Page`] by wrapping it and it's [`PagePool`].
///
/// Unlike [`Page`], this type handles deallocation for you upon dropping. It also provides a safe
/// way to access the contents of the page. However, the price for this convenience is that it is
/// heavier than the bare page type and cloning performs a deep copy.
pub struct FatPage {
    page_pool: PagePool,
    page: Page,
}

impl FatPage {
    /// See [`Page::as_ptr`].
    pub fn as_ptr(&self) -> *const u8 {
        self.page.as_ptr()
    }

    /// See [`Page::as_mut_ptr`].
    pub fn as_mut_ptr(&self) -> *mut u8 {
        self.page.as_mut_ptr()
    }

    /// Returns underlying [`Page`].
    ///
    /// Care must be taken that the returned page is not used after `self` is dropped. However,
    /// failing to do so will not cause safety bugs.
    pub fn page(&self) -> Page {
        self.page.clone()
    }
}

impl Clone for FatPage {
    fn clone(&self) -> Self {
        let mut new_page = self.page_pool.alloc_fat_page();
        new_page.copy_from_slice(&*self);
        new_page
    }
}

impl Deref for FatPage {
    type Target = [u8];

    fn deref(&self) -> &Self::Target {
        unsafe { self.page.as_mut_slice() }
    }
}

impl DerefMut for FatPage {
    fn deref_mut(&mut self) -> &mut Self::Target {
        unsafe { self.page.as_mut_slice() }
    }
}

impl Drop for FatPage {
    fn drop(&mut self) {
        self.page_pool.dealloc(self.page.clone());
    }
}

/// [`PagePool`] is an efficient allocator for pages used in IO operations.
///
/// It allows for efficient allocation and deallocation of pages.
#[derive(Clone)]
pub struct PagePool {
    inner: Arc<Inner>,
}

struct Inner {
    // `regions` is a preallocated string of regions. `n_regions` is the number of regions that are
    // currently allocated and thus the index of the first unallocated region. An unallocated region
    // has the value of `null`. `n_regions` only grows, never shrinks and cannot exceed
    // [`REGION_COUNT`]. Once a region is allocated, it will not be freed until the pool is dropped.
    // Moreover, the pointer stored in `regions[i]` where `i < n_regions` is immutable once set.
    regions: [AtomicPtr<u8>; REGION_COUNT],
    n_regions: AtomicU32,
    freelist: RwLock<Vec<Page>>,
    // The local freelist for the current thread used to avoid contention on the global freelist.
    tls_freelist: ThreadLocal<RefCell<Vec<Page>>>,
}

impl PagePool {
    /// Creates a new empty page pool.
    pub fn new() -> Self {
        let regions = std::array::from_fn(|_| AtomicPtr::new(std::ptr::null_mut()));
        // The capacity is chosen to be large enough to fit 4 times as much as 50k pages.
        let freelist = RwLock::new(Vec::with_capacity(200000));
        Self {
            inner: Arc::new(Inner {
                regions,
                n_regions: AtomicU32::new(0),
                freelist,
                tls_freelist: ThreadLocal::new(),
            }),
        }
    }

    /// Allocates a new [`FatPage`].
    pub fn alloc_fat_page(&self) -> FatPage {
        let page = self.alloc();
        FatPage {
            page_pool: self.clone(),
            page,
        }
    }

    /// Allocates a new [`Page`].
    ///
    /// The contents of the page are undefined.
    pub fn alloc(&self) -> Page {
        // fast path: try to serve request from the thread-local freelist.
        let mut tls_freelist = self.tls_freelist();
        if let Some(page) = tls_freelist.pop() {
            return page;
        }

        // if none is available, try to replenish the thread-local freelist from the global one.
        let mut freelist = self.inner.freelist.write();

        if freelist.len() < TLS_FREELIST_CAPACITY {
            // ensure that the global freelist has enough pages to refill the thread-local one.
            self.grow(&mut freelist);
            assert!(freelist.len() >= TLS_FREELIST_CAPACITY);
        }

        // transfer at most TLS_FREELIST_CAPACITY pages from the global freelist to the
        // thread-local freelist.
        tls_freelist.extend(freelist.drain(..TLS_FREELIST_CAPACITY));
        tls_freelist.pop().unwrap()
    }

    /// Deallocates a [`Page`].
    pub fn dealloc(&self, page: Page) {
        // fast path: try to place page in thread-local freelist.
        let mut tls_freelist = self.tls_freelist();
        tls_freelist.push(page);

        if tls_freelist.len() < TLS_FREELIST_CAPACITY * 2 {
            return;
        }

        // slow path: drain TLS free-list to global free-list.
        let mut freelist = self.inner.freelist.write();
        freelist.extend(tls_freelist.drain(TLS_FREELIST_CAPACITY..));
    }

    fn tls_freelist<'a>(&'a self) -> std::cell::RefMut<'a, Vec<Page>> {
        self.inner
            .tls_freelist
            .get_or(|| RefCell::new(Vec::with_capacity(TLS_FREELIST_CAPACITY)))
            .borrow_mut()
    }

    #[cold]
    fn grow(&self, freelist_guard: &mut RwLockWriteGuard<Vec<Page>>) {
        // First step is to allocate a new region.
        let region_ptr = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                REGION_BYTE_SIZE,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_PRIVATE | libc::MAP_ANONYMOUS,
                /* fd */ -1,
                /* offset */ 0,
            )
        };
        if region_ptr == libc::MAP_FAILED {
            panic!("Failed to allocate memory");
        }
        assert!(!region_ptr.is_null());

        // Next, we need to store the region pointer in the regions array.
        //
        // We store the pointer in the regions array before incrementing n_regions. This is not
        // strictly necessary, because the freelist is empty and no page can refer to the new region
        // yet. Likewise, drop cannot happen during this operation. We still do it in this order
        // to just err on the safe side and avoid any potential issues.
        //
        // Also, note the ordering is not really important here since we own the lock.
        let region_ix = self.inner.n_regions.load(Ordering::Relaxed);
        self.inner.regions[region_ix as usize].store(region_ptr as *mut u8, Ordering::Relaxed);
        self.inner.n_regions.fetch_add(1, Ordering::Release);

        // Finally, we need to populate the freelist with the pages in the new region.
        for slot in 0..SLOTS_PER_REGION {
            let page_ptr = unsafe { region_ptr.add(slot * PAGE_SIZE) } as *mut u8;
            freelist_guard.push(Page(page_ptr));
        }
    }
}

impl Drop for Inner {
    fn drop(&mut self) {
        for i in 0..self.n_regions.load(Ordering::Relaxed) as usize {
            let region_ptr = self.regions[i].load(Ordering::Relaxed);
            assert!(!region_ptr.is_null());
            unsafe {
                // SAFETY: `region_ptr` is a valid pointer to a region that was allocated and not
                // yet freed by this pool.
                libc::munmap(region_ptr as *mut libc::c_void, REGION_BYTE_SIZE);
            }
        }
    }
}

unsafe impl Send for PagePool {}
unsafe impl Sync for PagePool {}
