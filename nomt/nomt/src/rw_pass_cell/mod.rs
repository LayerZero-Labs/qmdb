//! A data structure similar to a `RwLock`, but with a twist: it can only be accessed when the
//! correct pass is shown. This is useful when you want to obtain a read or write lock on some
//! subset of data, that is not necessarily owned by a single object.
//!
//! The usage pattern is as follows:
//!
//! 1. Create a [`RwPassDomain`] with [`RwPassDomain::new()`].
//! 2. Protect the data you want to access with a [`RwPassCell`] by calling
//!    [`RwPassDomain::protect_with_id()`].
//! 3. Obtain a read or write pass by calling [`RwPassDomain::new_read_pass()`] or
//!    [`RwPassDomain::new_write_pass()`].
//! 4. Use the pass to access the data within any of [`RwPassCell`]-s created within the domain
//!    using the [`RwPassCell::read()`] or [`RwPassCell::write()`] methods.
//!
//! # Example
//!
//! ```ignore
//! # use nomt::rw_pass_cell::{RwPassDomain, RwPassCell, ReadPass, WritePass};
//!
//! let domain = RwPassDomain::new();
//! let cell: RwPassCell<usize> = domain.protect(42);
//! let read_pass = domain.new_read_pass();
//! assert_eq!(cell.read(&read_pass).get(), 42);
//! ```

#![allow(dead_code)]

#[cfg(loom)]
mod loom_tests;

#[cfg(loom)]
use loom::{
    cell::UnsafeCell,
    sync::atomic::{AtomicBool, AtomicUsize},
};

#[cfg(not(loom))]
use std::{
    cell::UnsafeCell,
    sync::atomic::{AtomicBool, AtomicUsize},
};

use std::{
    marker::PhantomData,
    ops::{Deref, DerefMut},
    sync::{atomic::Ordering, Arc, Weak},
};

use parking_lot::{RawRwLock, RwLock};

type RwLockReadGuard = parking_lot::lock_api::ArcRwLockReadGuard<RawRwLock, ()>;
type RwLockWriteGuard = parking_lot::lock_api::ArcRwLockWriteGuard<RawRwLock, ()>;
type Shared = RwLock<()>;

/// A domain that oversees [`RwPassCell`]s and provides read and write passes to access them.
#[derive(Clone)]
pub struct RwPassDomain {
    shared: Arc<Shared>,
}

impl RwPassDomain {
    /// Create a new [`RwPassDomain`]
    pub fn new() -> Self {
        Self {
            shared: Arc::new(RwLock::new(())),
        }
    }

    /// Protects the given inner value, along with an immutable identifier inside a [`RwPassCell`].
    pub fn protect_with_id<T, Id>(&self, inner: T, id: Id) -> RwPassCell<T, Id> {
        RwPassCell::new(Arc::downgrade(&self.shared), inner, id)
    }

    /// Creates a new read pass.
    ///
    /// The pass can be used to access the data within any [`RwPassCell`]s created within this
    /// domain.
    ///
    /// If there are any write passes active, this method will block until they are dropped.
    pub fn new_read_pass(&self) -> ReadPass {
        let guard = self.shared.read_arc();
        ReadPass {
            domain: self.shared.clone(),
            region: UniversalRegion,
            _guard: Arc::new(RwGuard::Read(guard)),
        }
    }

    /// Creates a new write pass.
    ///
    /// The pass can be used to access the data within any [`RwPassCell`]s created within this
    /// domain.
    ///
    /// If there are any read or write passes active, this method will block until they are dropped.
    pub fn new_write_pass(&self) -> WritePass {
        let guard = self.shared.write_arc();
        WritePass {
            parent: None,
            consumed: false,
            read_pass: ReadPass {
                domain: self.shared.clone(),
                region: UniversalRegion,
                _guard: Arc::new(RwGuard::Write(guard)),
            },
        }
    }
}

enum RwGuard {
    Read(#[allow(unused)] RwLockReadGuard),
    Write(#[allow(unused)] RwLockWriteGuard),
}

/// The Universal Region contains all IDs of all type but cannot be split.
#[derive(Debug, Clone, Copy)]
pub struct UniversalRegion;

/// A token that allows read access to the data within one domain.
pub struct ReadPass<R = UniversalRegion> {
    domain: Arc<Shared>,
    region: R,
    _guard: Arc<RwGuard>,
}

impl<R> ReadPass<R> {
    /// Get the underlying region of the pass.
    pub fn region(&self) -> &R {
        &self.region
    }
}

impl ReadPass<UniversalRegion> {
    /// Supply a region type.
    pub fn with_region<R>(self, region: R) -> ReadPass<R> {
        ReadPass {
            domain: self.domain.clone(),
            region,
            _guard: self._guard.clone(),
        }
    }
}

struct ParentWritePass<R> {
    parent: Option<Arc<ParentWritePass<R>>>,
    region: R,
    remaining_children: AtomicUsize,
}

/// A token that allows read-write access to the data within one domain.
pub struct WritePass<R = UniversalRegion> {
    parent: Option<Arc<ParentWritePass<R>>>,
    consumed: bool,
    read_pass: ReadPass<R>,
}

impl<R> WritePass<R> {
    /// Get the underlying region of the pass.
    pub fn region(&self) -> &R {
        self.read_pass.region()
    }

    /// Downgrades the write pass to a read pass.
    pub fn downgrade(&mut self) -> &ReadPass<R> {
        // `mut` because otherwise, it will be possible to create an alias.
        &self.read_pass
    }

    /// Wrap this in an envelope to be safely sent across threads.
    ///
    /// The [`WritePassEnvelope`] ensures that any writes to memory will be propagated
    /// to the remote thread before allowing the recipient to read or write.
    pub fn into_envelope(self) -> WritePassEnvelope<R> {
        // SAFETY: We release all writes from this thread before sending to another.
        //
        //         Arc here is probably overkill and boxing would be fine, but according to the
        //         rust docs: "A Rust atomic type that is exclusively owned or behind a mutable
        //         reference does not correspond to an “atomic object” in C++".
        //         (ref: https://doc.rust-lang.org/stable/std/sync/atomic/index.html)
        //
        //         Atomic variables that aren't at a stable reference and furthermore which aren't
        //         behind a _shared pointer_ are not required to emit atomic instructions. So we
        //         have to allocate and choose err on the side of caution.
        let sync_flag = Arc::new(AtomicBool::new(false));
        sync_flag.store(true, Ordering::Release);

        WritePassEnvelope {
            inner: self,
            sync_flag,
        }
    }
}

impl<R: Region + Clone> WritePass<R> {
    /// Split this write pass into N parts encompassing non-overlapping sub-regions.
    ///
    /// The result will be a vector corresponding to the input argument regions.
    ///
    /// This has O(n^2) complexity as a result of needing to check that each region is mutually
    /// exclusive with all others.
    ///
    /// An empty argument vector just drops `self` and returns an empty vector.
    ///
    /// # Panics
    ///
    /// Panics if any of the regions overlap with each other or are not encompassed by the
    /// region of this pass.
    pub fn split_n(mut self, regions: Vec<R>) -> Vec<Self> {
        if regions.len() == 0 {
            return Vec::new();
        }

        for (i, region) in regions.iter().enumerate() {
            assert!(self.region().encompasses(&region));

            for other_region in regions.iter().skip(i + 1) {
                assert!(region.excludes_unique(&other_region));
            }
        }

        let new_parent = Arc::new(ParentWritePass {
            parent: self.parent.take(),
            region: self.region().clone(),
            remaining_children: AtomicUsize::new(regions.len()),
        });

        regions
            .into_iter()
            .map(|region| WritePass {
                parent: Some(new_parent.clone()),
                consumed: false,
                read_pass: ReadPass {
                    domain: self.read_pass.domain.clone(),
                    region,
                    _guard: self.read_pass._guard.clone(),
                },
            })
            .collect()
    }

    /// Consume this regioned write-pass, possibly yielding the parent region back.
    ///
    /// If all other split descendents of the parent write pass have been consumed,
    /// this will return a region equivalent to the parent's.
    ///
    /// All write-passes need to be consumed before being dropped to yield the parent region.
    pub fn consume(mut self) -> Option<Self> {
        let Some(ref parent) = self.parent else {
            return None;
        };

        self.consumed = true;

        // SAFETY: release our writes to other threads if _not_ the last sibling.
        //         acquire writes from other threads if the last sibling.
        //         the value of '1' must have been written by another thread with `Release`.
        if parent.remaining_children.fetch_sub(1, Ordering::AcqRel) == 1 {
            Some(WritePass {
                parent: parent.parent.clone(),
                consumed: false,
                read_pass: ReadPass {
                    domain: self.read_pass.domain.clone(),
                    region: parent.region.clone(),
                    _guard: self.read_pass._guard.clone(),
                },
            })
        } else {
            None
        }
    }
}

impl<R> Drop for WritePass<R> {
    fn drop(&mut self) {
        if let Some(ref parent) = self.parent {
            if !self.consumed {
                // SAFETY: release our writes to other threads.
                parent.remaining_children.fetch_sub(1, Ordering::Release);
            }
        }
    }
}

impl WritePass<UniversalRegion> {
    /// Supply a region type.
    pub fn with_region<R>(self, region: R) -> WritePass<R> {
        // sanity: UniversalRegion can't be split, so this should never be Some.
        assert!(self.parent.is_none());

        WritePass {
            parent: None,
            consumed: false,
            read_pass: ReadPass {
                domain: self.read_pass.domain.clone(),
                region,
                _guard: self.read_pass._guard.clone(),
            },
        }
    }
}

/// A wrapper around a [`WritePass`] which can be sent between threads.
///
/// The reason for this type is that `WritePass` is not safe to send between threads.
pub struct WritePassEnvelope<R = UniversalRegion> {
    inner: WritePass<R>,
    sync_flag: Arc<AtomicBool>,
}

impl<R> WritePassEnvelope<R> {
    /// Open the envelope, yielding a write pass to be used on another thread.
    pub fn into_inner(self) -> WritePass<R> {
        // SAFETY: acquire writes from the sending thread.
        //         while this is technically a spin loop, it should be rare for it to hang for long,
        //         since this flag is set before the envelope is even created. so spinning
        //         at all requires some substantial reordering + unfortunate context switching away
        //         from the sending thread.
        while !self.sync_flag.load(Ordering::Acquire) {
            std::hint::spin_loop();
        }
        self.inner
    }
}

/// SAFETY: this is safe to send because we deliberately read propagated writes from other threads
///         and all regioned write pass drops and envelope creations propagate those writes.
///         The cell itself protects the data from being sent where not valid.
unsafe impl<R: Send> Send for WritePassEnvelope<R> {}

/// A cell corresponding with a [`RwPassDomain`]. This may be read and written only with a pass from
/// the domain.
pub struct RwPassCell<T, Id = ()> {
    // A weak reference to the creator of the cell.
    //
    // It's weak because we don't actually care about it's internal data, we just need to make
    // sure that the creator is alive so that the address is valid. Otherwise, the address could
    // be reused, and the provenance check would be invalid.
    provenance: Weak<Shared>,
    inner: UnsafeCell<T>,
    id: Id,
}

impl<T, Id> RwPassCell<T, Id> {
    fn new(provenance: Weak<Shared>, inner: T, id: Id) -> Self {
        Self {
            provenance,
            inner: UnsafeCell::new(inner),
            id,
        }
    }

    /// Returns a handle to read the value. Requires showing a read pass.
    ///
    /// Panics if the provided pass belongs to a different [`RwPassDomain`] than the cell.
    /// If this pass is constrained to a specific region, this panics if the item's ID is outside
    /// of the region's exclusive or non-exclusive access range.
    pub fn read<'a, 'pass, R: RegionContains<Id>>(
        &'a self,
        read_pass: &'pass ReadPass<R>,
    ) -> ReadGuard<'a, 'pass, T> {
        self.check_domain(&read_pass.domain);
        assert!(read_pass.region().contains(&self.id));
        ReadGuard {
            inner: &self.inner,
            _read_pass: PhantomData,
        }
    }

    /// Returns a handle to write the value. Requires showing a write pass.
    ///
    /// Panics if the provided pass belongs to a different [`RwPassDomain`] than the cell.
    /// If this pass is constrained to a specific region, this panics if the item's ID is outside
    /// of the region's exclusive access range.
    pub fn write<'a, 'pass, R: RegionContains<Id>>(
        &'a self,
        write_pass: &'pass mut WritePass<R>,
    ) -> WriteGuard<'a, 'pass, T> {
        self.check_domain(&write_pass.read_pass.domain);
        assert!(write_pass.region().contains_exclusive(&self.id));
        WriteGuard {
            inner: &self.inner,
            _write_pass: PhantomData,
        }
    }

    /// Prevents any funny business by ensuring the domain of the pass and the domain of the cell
    /// match.
    fn check_domain(&self, domain: &Arc<Shared>) {
        let domain_ptr = Arc::as_ptr(domain);
        let provenance_ptr = Weak::as_ptr(&self.provenance);
        assert!(std::ptr::eq(domain_ptr, provenance_ptr), "Domain mismatch");
    }
}

// SAFETY: The RwPassCell is Send and Sync if the inner value is Send and Sync. This is because the
//         undelying value is protected by the RwPassDomain, which ensures that the value is only
//         accessed when the correct pass is shown.
// SAFETY: The RwPasscell is Send if the inner value is Send and the Id is Sync. The underlying
//         value is protected by the RwPassDomain, but mutable exclusive access of the value
//         may occur on multiple threads. The Id is read-only.
unsafe impl<T: Send, Id: Sync> Send for RwPassCell<T, Id> {}
// SAFETY: The RwPasscell is Sync if both the inner value the Id are Sync.
//         They may both be read simultaneously from multiple threads if the appropriate passes
//         are shown. This also requires T to be Send because multiple threads can take mutable
//         access to the inner type in turn through a shared reference to the cell.
unsafe impl<T: Sync + Send, Id: Sync> Sync for RwPassCell<T, Id> {}

/// A read guard for the value of an [`RwPassCell`]. This may exist concurrently with other
/// readers.
// SAFETY: this cannot be `Clone`.
pub struct ReadGuard<'a, 'pass, T> {
    inner: &'a UnsafeCell<T>,
    _read_pass: PhantomData<&'pass ReadPass>,
}

impl<'a, 'pass, T> ReadGuard<'a, 'pass, T>
where
    'pass: 'a,
{
    #[cfg(not(loom))]
    /// Get a reference to the underlying value.
    pub fn get(&self) -> &'pass T {
        // SAFETY: The existence of the guard ensures that there are only shared references
        //         to the inner value. The returned reference cannot outlive the guard.
        unsafe { &*self.inner.get() }
    }

    #[cfg(loom)]
    fn with<R>(&self, f: impl FnOnce(&T) -> R) -> R {
        // SAFETY: The existence of the guard ensures that there is only one mutable reference
        //         to the inner value. The returned reference cannot outlive the guard.
        self.inner.with(|inner| f(unsafe { &*inner }))
    }
}

impl<'a, 'pass, T> Deref for ReadGuard<'a, 'pass, T> {
    type Target = T;

    #[cfg(not(loom))]
    fn deref(&self) -> &Self::Target {
        self.get()
    }

    #[cfg(loom)]
    fn deref(&self) -> &Self::Target {
        unreachable!("with loom cfg, ReadGuard::deref cannot be used")
    }
}

/// A read guard for the value of an [`RwPassCell`]. This may exist concurrently with other
/// readers.
pub struct WriteGuard<'a, 'pass: 'a, T> {
    inner: &'a UnsafeCell<T>,
    _write_pass: PhantomData<&'pass mut WritePass>,
}

impl<'a, 'pass, T> WriteGuard<'a, 'pass, T>
where
    'pass: 'a,
{
    #[cfg(not(loom))]
    /// Get a reference to the underlying value.
    pub fn get(&self) -> &T {
        // SAFETY: The existence of the guard ensures that there is only one mutable reference
        //         to the inner value. The returned reference cannot outlive the guard.
        unsafe { &*self.inner.get() }
    }

    #[cfg(loom)]
    fn with<R>(&self, f: impl FnOnce(&T) -> R) -> R {
        // SAFETY: The existence of the guard ensures that there is only one mutable reference
        //         to the inner value. The returned reference cannot outlive the guard.
        self.inner.with(|inner| f(unsafe { &*inner }))
    }

    #[cfg(not(loom))]
    /// Get a mutable reference to the underlying value.
    pub fn get_mut(&mut self) -> &mut T {
        // SAFETY: The existence of the mutable guard reference ensures that there is only one
        //         mutable reference to the inner value. The returned reference cannot outlive the
        //         guard.
        unsafe { &mut *self.inner.get() }
    }

    #[cfg(loom)]
    fn with_mut<R>(&self, f: impl FnOnce(&mut T) -> R) -> R {
        // SAFETY: The existence of the guard ensures that there is only one mutable reference
        //         to the inner value. The returned reference cannot outlive the guard.
        self.inner.with_mut(|inner| f(unsafe { &mut *inner }))
    }
}

impl<'a, 'pass, T> Deref for WriteGuard<'a, 'pass, T> {
    type Target = T;

    #[cfg(not(loom))]
    fn deref(&self) -> &Self::Target {
        self.get()
    }

    #[cfg(loom)]
    fn deref(&self) -> &Self::Target {
        unreachable!("with loom cfg, WriteGuard::deref cannot be used")
    }
}

impl<'a, 'pass, T> DerefMut for WriteGuard<'a, 'pass, T> {
    #[cfg(not(loom))]
    fn deref_mut(&mut self) -> &mut Self::Target {
        self.get_mut()
    }

    #[cfg(loom)]
    fn deref_mut(&mut self) -> &mut Self::Target {
        unreachable!("with loom cfg, WriteGuard::deref_mut cannot be used")
    }
}

/// Whether a region contains values or not.
///
/// This trait is unsafe because the memory safety of the [`RwPassCell`]
/// depends on it being implemented accurately, as well as on the Region trait being
/// implemented correctly.
///
/// Safely implementing this trait means correctly implementing the functions and choosing an ID
/// type which cannot break the invariants of the trait. Plain-Old Data types which do not support
/// interior mutability are good choices for ID.
pub unsafe trait RegionContains<Id>: Region {
    /// Whether the region contains a ID, exclusively or not.
    ///
    /// # Safety
    ///
    /// If this function is ever invoked with an ID, it must return the same result whenever it is
    /// called in the future.
    fn contains(&self, id: &Id) -> bool;

    /// Whether the region contains a ID exclusively.
    ///
    /// # Safety
    ///
    /// If this function is ever invoked with an ID, it must return the same result whenever it is
    /// called in the future.
    fn contains_exclusive(&self, id: &Id) -> bool;
}

/// `Region`s, in conjunction with the [`RegionContains`] trait, expose a set-like abstraction
/// over ranges of data identifiers.
pub trait Region {
    /// Whether the region completely encompasses another region.
    ///
    /// # Safety
    ///
    /// If this returns true, then it must be the case that this region contains every ID the other
    /// region does, for both shared and exclusive access.
    fn encompasses(&self, other: &Self) -> bool;

    /// Whether the region has no exclusive access overlaps with another region.
    ///
    /// # Safety
    ///
    /// If this returns true, then it must be the case that this region does not contain any ID
    /// for exclusive access that the other region contains at all, and vice versa.
    ///
    /// It is safe for both regions to have read-only access to an ID as long as neither
    /// has write access to it.
    fn excludes_unique(&self, other: &Self) -> bool;
}

// SAFETY: The Universal Region is safe because it has a single value, which does not exclude itself.
impl Region for UniversalRegion {
    fn encompasses(&self, _: &UniversalRegion) -> bool {
        true
    }

    fn excludes_unique(&self, _: &UniversalRegion) -> bool {
        false
    }
}

// SAFETY: The Universal Region is safe because it has a single value, which does not exclude itself.
unsafe impl<Id> RegionContains<Id> for UniversalRegion {
    fn contains(&self, _: &Id) -> bool {
        true
    }

    fn contains_exclusive(&self, _: &Id) -> bool {
        true
    }
}
