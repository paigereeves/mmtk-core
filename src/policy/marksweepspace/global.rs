use std::{
    sync::{Arc, atomic::{AtomicUsize, AtomicU32}, Mutex}, marker::PhantomData, collections::HashMap,
};

use atomic::Ordering;

use crate::{TransitiveClosure, policy::{marksweepspace::{block::{Block, BlockState}, chunks::Chunk, metadata::{is_marked, set_mark_bit}}, space::SpaceOptions, mallocspace::metadata::map_meta_space}, scheduler::{GCWorkScheduler, WorkBucketStage}, util::{Address, ObjectReference, OpaquePointer, VMThread, VMWorkerThread, alloc::free_list_allocator::{self, FreeListAllocator, BLOCK_LISTS_EMPTY, BYTES_IN_BLOCK}, alloc_bit::{ALLOC_SIDE_METADATA_SPEC, bzero_alloc_bit}, constants::LOG_BYTES_IN_PAGE, heap::{
            layout::heap_layout::{Mmapper, VMMap},
            FreeListPageResource, HeapMeta, VMRequest,
        }, metadata::{self, MetadataSpec, side_metadata::{self, SideMetadataContext, SideMetadataSpec}, store_metadata}}, vm::VMBinding};

use super::{super::space::{CommonSpace, Space, SFT}, chunks::{ChunkMap, ChunkState}};
use crate::vm::ObjectModel;
use crate::{policy::marksweepspace::metadata::{set_page_mark}, util::conversions};
use crate::util::alloc_bit::{is_alloced, set_alloc_bit, unset_alloc_bit_unsafe};
use super::is_alloced_by_malloc;
use crate::util::metadata::side_metadata::SideMetadataSanity;
use super::metadata::*;
use crate::util::constants::BYTES_IN_PAGE;
use crate::util::heap::PageResource;
use crate::util::malloc::*;
use crate::util::metadata::side_metadata::{
    bzero_metadata,
};
use crate::util::opaque_pointer::*;
use crate::vm::{ActivePlan, Collection};
use crate::{util::heap::layout::vm_layout_constants::BYTES_IN_CHUNK};

// If true, we will use a hashmap to store all the allocated memory from malloc, and use it
// to make sure our allocation is correct.
#[cfg(all(feature="malloc", debug_assertions))]
const ASSERT_ALLOCATION: bool = false;


#[cfg(feature="malloc")]
pub struct MarkSweepSpace<VM: VMBinding> {
    phantom: PhantomData<VM>,
    active_bytes: AtomicUsize,
    pub chunk_addr_min: AtomicUsize, // XXX: have to use AtomicUsize to represent an Address
    pub chunk_addr_max: AtomicUsize,
    metadata: SideMetadataContext,
    // Mapping between allocated address and its size - this is used to check correctness.
    // Size will be set to zero when the memory is freed.
    #[cfg(debug_assertions)]
    active_mem: Mutex<HashMap<Address, usize>>,
    // The following fields are used for checking correctness of the parallel sweep implementation
    // as we need to check how many live bytes exist against `active_bytes` when the last sweep
    // work packet is executed
    #[cfg(debug_assertions)]
    pub total_work_packets: AtomicU32,
    #[cfg(debug_assertions)]
    pub completed_work_packets: AtomicU32,
    #[cfg(debug_assertions)]
    pub work_live_bytes: AtomicUsize,
}

#[cfg(not(feature="malloc"))]
pub struct MarkSweepSpace<VM: VMBinding> {
    pub common: CommonSpace<VM>,
    pr: FreeListPageResource<VM>,
    /// Allocation status for all chunks in MS space
    pub chunk_map: ChunkMap,
    /// Work packet scheduler
    scheduler: Arc<GCWorkScheduler<VM>>,
}

impl<VM: VMBinding> SFT for MarkSweepSpace<VM> {
    fn name(&self) -> &str {
        self.get_name()
    }

    fn is_live(&self, object: crate::util::ObjectReference) -> bool {
        is_marked::<VM>(object, Some(Ordering::SeqCst))
    }

    fn is_movable(&self) -> bool {
        false
    }

    #[cfg(feature = "sanity")]
    fn is_sane(&self) -> bool {
        true
    }

    #[cfg(feature="malloc")]
    fn initialize_object_metadata(&self, object: crate::util::ObjectReference, alloc: bool) {
        trace!("initialize_object_metadata for object {}", object);
        let page_addr = conversions::page_align_down(object.to_address());
        set_page_mark(page_addr);
        set_alloc_bit(object);
    }

    #[cfg(not(feature="malloc"))]
    fn initialize_object_metadata(&self, object: crate::util::ObjectReference, alloc: bool) {
        trace!("initialize_object_metadata for object {}", object);
        set_alloc_bit(object);
    }
}

impl<VM: VMBinding> Space<VM> for MarkSweepSpace<VM> {
    fn as_space(&self) -> &dyn Space<VM> {
        self
    }

    fn as_sft(&self) -> &(dyn SFT + Sync + 'static) {
        self
    }

    fn get_page_resource(&self) -> &dyn crate::util::heap::PageResource<VM> {
        &self.pr
    }

    fn common(&self) -> &CommonSpace<VM> {
        &self.common
    }

    fn init(&mut self, vm_map: &'static crate::util::heap::layout::heap_layout::VMMap) {
        #[cfg(not(feature="malloc"))]
        self.common().init(self.as_space());
    }

    fn release_multiple_pages(&mut self, start: crate::util::Address) {
        #[cfg(not(feature="malloc"))]
        self.pr.release_pages(start);
    }

    // We have assertions in a debug build. We allow this pattern for the release build.
    #[cfg(feature="malloc")]
    #[allow(clippy::let_and_return)]
    fn in_space(&self, object: ObjectReference) -> bool {

        let ret = is_alloced_by_malloc(object);

        #[cfg(debug_assertions)]
        if ASSERT_ALLOCATION {
            let addr = VM::VMObjectModel::object_start_ref(object);
            let active_mem = self.active_mem.lock().unwrap();
            if ret {
                // The alloc bit tells that the object is in space.
                debug_assert!(
                    *active_mem.get(&addr).unwrap() != 0,
                    "active mem check failed for {} (object {}) - was freed",
                    addr,
                    object
                );
            } else {
                // The alloc bit tells that the object is not in space. It could never be allocated, or have been freed.
                debug_assert!(
                    (!active_mem.contains_key(&addr))
                        || (active_mem.contains_key(&addr) && *active_mem.get(&addr).unwrap() == 0),
                    "mem check failed for {} (object {}): allocated = {}, size = {:?}",
                    addr,
                    object,
                    active_mem.contains_key(&addr),
                    if active_mem.contains_key(&addr) {
                        active_mem.get(&addr)
                    } else {
                        None
                    }
                );
            }
        }
        ret
    }

    #[cfg(feature="malloc")]
    fn address_in_space(&self, _start: Address) -> bool {
        unreachable!("We do not know if an address is in the mark sweep space if we are using malloc to allocate. Use in_space() to check if an object is in the space.")
    }

    fn get_name(&self) -> &'static str {
        "MarkSweepSpace"
    }

    #[cfg(feature="malloc")]
    fn reserved_pages(&self) -> usize {
        // TODO: figure out a better way to get the total number of active pages from the metadata
        let data_pages = conversions::bytes_to_pages_up(self.active_bytes.load(Ordering::SeqCst));
        let meta_pages = self.metadata.calculate_reserved_pages(data_pages);
        data_pages + meta_pages
    }

    #[cfg(feature="malloc")]
    fn verify_side_metadata_sanity(&self, side_metadata_sanity_checker: &mut SideMetadataSanity) {

        side_metadata_sanity_checker
            .verify_metadata_context(std::any::type_name::<Self>(), &self.metadata)
    }
}

impl<VM: VMBinding> MarkSweepSpace<VM> {
    /// Get work packet scheduler
    #[cfg(not(feature="malloc"))]
    fn scheduler(&self) -> &GCWorkScheduler<VM> {
        &self.scheduler
    }

    #[cfg(feature="malloc")]
    pub fn new(global_side_metadata_specs: Vec<SideMetadataSpec>) -> Self {
        use crate::policy::{marksweepspace::metadata::ACTIVE_PAGE_METADATA_SPEC};

        MarkSweepSpace {
            phantom: PhantomData,
            active_bytes: AtomicUsize::new(0),
            chunk_addr_min: AtomicUsize::new(usize::max_value()), // XXX: have to use AtomicUsize to represent an Address
            chunk_addr_max: AtomicUsize::new(0),
            metadata: SideMetadataContext {
                global: global_side_metadata_specs,
                local: metadata::extract_side_metadata(&[
                    MetadataSpec::OnSide(ACTIVE_PAGE_METADATA_SPEC),
                    *VM::VMObjectModel::LOCAL_MARK_BIT_SPEC,
                ]),
            },
            #[cfg(debug_assertions)]
            active_mem: Mutex::new(HashMap::new()),
            #[cfg(debug_assertions)]
            total_work_packets: AtomicU32::new(0),
            #[cfg(debug_assertions)]
            completed_work_packets: AtomicU32::new(0),
            #[cfg(debug_assertions)]
            work_live_bytes: AtomicUsize::new(0),
        }
    }

    #[cfg(not(feature="malloc"))]
    pub fn new(
        name: &'static str,
        zeroed: bool,
        vmrequest: VMRequest,
        // local_side_metadata_specs: Vec<SideMetadataSpec>,
        vm_map: &'static VMMap,
        mmapper: &'static Mmapper,
        heap: &mut HeapMeta,
        scheduler: Arc<GCWorkScheduler<VM>>,
    ) -> MarkSweepSpace<VM> {
        let alloc_bits = &mut metadata::extract_side_metadata(&[
            MetadataSpec::OnSide(ALLOC_SIDE_METADATA_SPEC),
        ]);

        let mark_bits = &mut metadata::extract_side_metadata(&[
            *VM::VMObjectModel::LOCAL_MARK_BIT_SPEC,
        ]);

        let mut local_specs = {
            metadata::extract_side_metadata(
            &vec![
                MetadataSpec::OnSide(Block::NEXT_BLOCK_TABLE),
                MetadataSpec::OnSide(Block::PREV_BLOCK_TABLE),
                MetadataSpec::OnSide(Block::FREE_LIST_TABLE),
                MetadataSpec::OnSide(Block::SIZE_TABLE),
                MetadataSpec::OnSide(Block::LOCAL_FREE_LIST_TABLE),
                MetadataSpec::OnSide(Block::THREAD_FREE_LIST_TABLE),
                MetadataSpec::OnSide(Block::BLOCK_LIST_TABLE),
                MetadataSpec::OnSide(Block::TLS_TABLE),
                MetadataSpec::OnSide(Block::MARK_TABLE),
                MetadataSpec::OnSide(ChunkMap::ALLOC_TABLE),
                *VM::VMObjectModel::LOCAL_MARK_BIT_SPEC,
            ]
            )
        };

        local_specs.append(mark_bits);

        let common = CommonSpace::new(
            SpaceOptions {
                name,
                movable: false,
                immortal: false,
                needs_log_bit: false,
                zeroed,
                vmrequest,
                side_metadata_specs: SideMetadataContext {
                    global: alloc_bits.to_vec(),
                    local: local_specs,
                },
            },
            vm_map,
            mmapper,
            heap,
        );
        MarkSweepSpace {
            pr: if vmrequest.is_discontiguous() {
                FreeListPageResource::new_discontiguous(0, vm_map)
            } else {
                FreeListPageResource::new_contiguous(common.start, common.extent, 0, vm_map)
            },
            common,
            chunk_map: ChunkMap::new(),
            scheduler,
        }
    }

    #[cfg(feature="malloc")]
    pub fn alloc(&self, tls: VMThread, size: usize) -> Address {

        // TODO: Should refactor this and Space.acquire()
        if VM::VMActivePlan::global().poll(false, self) {
            assert!(VM::VMActivePlan::is_mutator(tls), "Polling in GC worker");
            VM::VMCollection::block_for_gc(VMMutatorThread(tls));
            return unsafe { Address::zero() };
        }

        let raw = unsafe { calloc(1, size) };
        let address = Address::from_mut_ptr(raw);

        if !address.is_zero() {
            let actual_size = unsafe { malloc_usable_size(raw) };
            // If the side metadata for the address has not yet been mapped, we will map all the side metadata for the range [address, address + actual_size).
            if !is_meta_space_mapped(address, actual_size) {
                // Map the metadata space for the associated chunk
                self.map_metadata_and_update_bound(address, actual_size);
            }
            self.active_bytes.fetch_add(actual_size, Ordering::SeqCst);

            #[cfg(debug_assertions)]
            if ASSERT_ALLOCATION {
                debug_assert!(actual_size != 0);
                self.active_mem.lock().unwrap().insert(address, actual_size);
            }
        }

        address
    }
    #[cfg(feature="malloc")]
    // XXX optimize: We pass the bytes in to free as otherwise there were multiple
    // indirect call instructions in the generated assembly
    pub fn free(&self, addr: Address, bytes: usize) {
        let ptr = addr.to_mut_ptr();
        trace!("Free memory {:?}", ptr);
        unsafe {
            free(ptr);
        }
        self.active_bytes.fetch_sub(bytes, Ordering::SeqCst);

        #[cfg(debug_assertions)]
        if ASSERT_ALLOCATION {
            self.active_mem.lock().unwrap().insert(addr, 0).unwrap();
        }
    }

    #[inline]
    pub fn trace_object<T: TransitiveClosure>(
        &self,
        trace: &mut T,
        object: ObjectReference,
    ) -> ObjectReference {
        if object.is_null() {
            return object;
        }
        let address = object.to_address();
        assert!(
            self.in_space(object),
            "Cannot mark an object {} that was not alloced to the mark sweep space.",
            address,
        );

        if !is_marked::<VM>(object, Some(Ordering::SeqCst)) {
            set_mark_bit::<VM>(object, Some(Ordering::SeqCst));
            #[cfg(feature="malloc")]
            {
                let chunk_start = conversions::chunk_align_down(address);
                set_chunk_mark(chunk_start);
            }
            #[cfg(not(feature="malloc"))]
            {
                let block = FreeListAllocator::<VM>::get_block(address);
                block.set_state(BlockState::Marked);
            }
            trace.process_node(object);
        }
        object
    }
        
    #[cfg(not(feature="malloc"))]
    pub fn zero_mark_bits(&self) {
        // TODO: zero mark bits concurrently
        use crate::vm::*;
        for chunk in self.chunk_map.all_chunks() {
            if let MetadataSpec::OnSide(side) = *VM::VMObjectModel::LOCAL_MARK_BIT_SPEC {
                side_metadata::bzero_metadata(&side, chunk.start(), Chunk::BYTES);
            }
        }
    }

    #[cfg(not(feature="malloc"))]
    pub fn record_new_block(&self, block: Block) {
        block.init();
        self.chunk_map.set(block.chunk(), ChunkState::Allocated);
    }

    #[inline]
    #[cfg(not(feature="malloc"))]
    pub fn get_next_metadata_spec(&self) -> SideMetadataSpec {
        Block::NEXT_BLOCK_TABLE
    }

    #[cfg(not(feature="malloc"))]
    pub fn reset(&mut self) {
        self.zero_mark_bits();
    }

    #[cfg(not(feature="malloc"))]
    pub fn block_level_sweep(&self) {
        let space = unsafe { &*(self as *const Self) };
        for chunk in self.chunk_map.all_chunks() {
            chunk.sweep(space);
        }
        // let work_packets = self.chunk_map.generate_sweep_tasks(space);
        // self.scheduler().work_buckets[WorkBucketStage::Release].bulk_add(work_packets);
    }

    /// Release a block.
    #[cfg(not(feature="malloc"))]
    pub fn release_block(&self, block: Block) {
        // eprintln!("b < {}", block.start());
        self.block_clear_metadata(block);

        block.deinit();
        self.pr.release_pages(block.start());
    }

    #[cfg(not(feature="malloc"))]
    pub fn block_clear_metadata(&self, block: Block) {
        for metadata_spec in &self.common.metadata.local {
            store_metadata::<VM>(
                &MetadataSpec::OnSide(*metadata_spec),
                unsafe { block.start().to_object_reference() },
                0,
                None,
                Some(Ordering::SeqCst),
            )
        }
        bzero_alloc_bit(block.start(), BYTES_IN_BLOCK);
    }

    #[cfg(feature="malloc")]
    fn map_metadata_and_update_bound(&self, addr: Address, size: usize) {
        // Map the metadata space for the range [addr, addr + size)
        map_meta_space(&self.metadata, addr, size);

        // Update the bounds of the max and min chunk addresses seen -- this is used later in the sweep
        // Lockless compare-and-swap loops perform better than a locking variant

        // Update chunk_addr_min, basing on the start of the allocation: addr.
        {
            let min_chunk_start = conversions::chunk_align_down(addr);
            let min_chunk_usize = min_chunk_start.as_usize();
            let mut min = self.chunk_addr_min.load(Ordering::Relaxed);
            while min_chunk_usize < min {
                match self.chunk_addr_min.compare_exchange_weak(
                    min,
                    min_chunk_usize,
                    Ordering::AcqRel,
                    Ordering::Relaxed,
                ) {
                    Ok(_) => break,
                    Err(x) => min = x,
                }
            }
        }

        // Update chunk_addr_max, basing on the end of the allocation: addr + size.
        {
            let max_chunk_start = conversions::chunk_align_down(addr + size);
            let max_chunk_usize = max_chunk_start.as_usize();
            let mut max = self.chunk_addr_max.load(Ordering::Relaxed);
            while max_chunk_usize > max {
                match self.chunk_addr_max.compare_exchange_weak(
                    max,
                    max_chunk_usize,
                    Ordering::AcqRel,
                    Ordering::Relaxed,
                ) {
                    Ok(_) => break,
                    Err(x) => max = x,
                }
            }
        }
    }

    #[cfg(feature="malloc")]
    pub fn sweep_chunk(&self, chunk_start: Address) {
        // Call the relevant sweep function depending on the location of the mark bits
        match *VM::VMObjectModel::LOCAL_MARK_BIT_SPEC {
            MetadataSpec::OnSide(local_mark_bit_side_spec) => {
                self.sweep_chunk_mark_on_side(chunk_start, local_mark_bit_side_spec);
            }
            _ => {
                self.sweep_chunk_mark_in_header(chunk_start);
            }
        }
    }

    /// This function is called when the mark bits sit on the side metadata.
    /// This has been optimized with the use of bulk loading and bulk zeroing of
    /// metadata.
    ///
    /// This function uses non-atomic accesses to side metadata (although these
    /// non-atomic accesses should not have race conditions associated with them)
    /// as well as calls libc functions (`malloc_usable_size()`, `free()`)
    #[cfg(feature="malloc")]
    fn sweep_chunk_mark_on_side(&self, chunk_start: Address, mark_bit_spec: SideMetadataSpec) {
        #[cfg(debug_assertions)]
        let mut live_bytes = 0;

        debug!("Check active chunk {:?}", chunk_start);
        let mut chunk_is_empty = true;
        let mut address = chunk_start;
        let chunk_end = chunk_start + BYTES_IN_CHUNK;
        let mut page = conversions::page_align_down(address);
        let mut page_is_empty = true;
        let mut last_on_page_boundary = false;

        debug_assert!(
            crate::util::alloc_bit::ALLOC_SIDE_METADATA_SPEC.log_min_obj_size
                == mark_bit_spec.log_min_obj_size,
            "Alloc-bit and mark-bit metadata have different minimum object sizes!"
        );

        // For bulk xor'ing 128-bit vectors on architectures with vector instructions
        // Each bit represents an object of LOG_MIN_OBJ_SIZE size
        let bulk_load_size: usize =
            128 * (1 << crate::util::alloc_bit::ALLOC_SIDE_METADATA_SPEC.log_min_obj_size);

        while address < chunk_end {
            // We extensively tested the performance of the following if-statement and were
            // surprised to note that in the case of newer AMD microarchitecures (>= Zen), some
            // microarchitectural state/idiosyncrasies result in favourable cache placement/locality
            // for the case where the conditionals (i.e. just the body of both the if-statements are left
            // in the hot loop) which lead to a large performance speedup. Even more surprising was the
            // revelation that the hot loop has worse cache placement/locality if the entire if-statement
            // was commented out -- effectively meaning that [more work in the hot loop => better performance]
            // which was counterintuitive to our beliefs.
            //
            // The performance tradeoffs on Intel and older AMD microarchitectures were as expected, i.e.
            // wherein the performance of the hot loop decreased if more work was done in the loop.
            if address - page >= BYTES_IN_PAGE {
                if page_is_empty {
                    unsafe { unset_page_mark_unsafe(page) };
                }
                page = conversions::page_align_down(address);
                page_is_empty = !last_on_page_boundary;
                last_on_page_boundary = false;
            }

            let alloc_128: u128 =
                unsafe { load128(&crate::util::alloc_bit::ALLOC_SIDE_METADATA_SPEC, address) };
            let mark_128: u128 = unsafe { load128(&mark_bit_spec, address) };

            // Check if there are dead objects in the bulk loaded region
            if alloc_128 ^ mark_128 != 0 {
                let end = address + bulk_load_size;
                // Linearly scan through region to free dead objects
                while address < end {
                    trace!("Checking address = {}, end = {}", address, end);
                    if address - page >= BYTES_IN_PAGE {
                        if page_is_empty {
                            unsafe { unset_page_mark_unsafe(page) };
                        }
                        page = conversions::page_align_down(address);
                        page_is_empty = !last_on_page_boundary;
                        last_on_page_boundary = false;
                    }

                    // Check if the address is an object
                    if unsafe { is_alloced_object_unsafe(address) } {
                        let object = unsafe { address.to_object_reference() };
                        let obj_start = VM::VMObjectModel::object_start_ref(object);
                        let bytes = unsafe { malloc_usable_size(obj_start.to_mut_ptr()) };

                        if !is_marked::<VM>(object, None) {
                            // Dead object
                            trace!("Object {} has been allocated but not marked", object);

                            // Free object
                            self.free(obj_start, bytes);
                            trace!("free object {}", object);
                            unsafe { unset_alloc_bit_unsafe(object) };
                        } else {
                            // Live object
                            // This chunk and page are still active.
                            chunk_is_empty = false;
                            page_is_empty = false;

                            if address + bytes - page > BYTES_IN_PAGE {
                                last_on_page_boundary = true;
                            }
                        }

                        // Skip to next object
                        address += bytes;
                    } else {
                        // not an object
                        address += VM::MIN_ALIGNMENT;
                    }
                }
            } else {
                // TODO we aren't actually accounting for the case where an object is alive and spans
                // a page boundary as we don't know what the object sizes are/what is alive in the bulk region
                if alloc_128 != 0 {
                    // For the chunk/page to be alive, both alloc128 and mark128 values need to be not zero
                    chunk_is_empty = false;
                    page_is_empty = false;
                }

                address += bulk_load_size;
            }

            // Aligning addresses to `bulk_load_size` just makes life easier, even though
            // we may be processing some addresses twice
            address = address.align_down(bulk_load_size);
        }

        #[cfg(debug_assertions)]
        {
            let mut address = chunk_start;
            while address < chunk_end {
                // Check if the address is an object
                if unsafe { is_alloced_object_unsafe(address) } {
                    let object = unsafe { address.to_object_reference() };
                    let obj_start = VM::VMObjectModel::object_start_ref(object);
                    let bytes = unsafe { malloc_usable_size(obj_start.to_mut_ptr()) };

                    #[cfg(debug_assertions)]
                    if ASSERT_ALLOCATION {
                        debug_assert!(
                            self.active_mem.lock().unwrap().contains_key(&obj_start),
                            "Address {} with alloc bit is not in active_mem",
                            obj_start
                        );
                        debug_assert_eq!(
                            self.active_mem.lock().unwrap().get(&obj_start),
                            Some(&bytes),
                            "Address {} size in active_mem does not match the size from malloc_usable_size",
                            obj_start
                        );
                    }

                    assert!(
                        is_marked::<VM>(object, None),
                        "Dead object = {} found after sweep",
                        object
                    );

                    live_bytes += bytes;

                    // Skip to next object
                    address += bytes;
                } else {
                    // not an object
                    address += VM::MIN_ALIGNMENT;
                }
            }
        }

        // Clear all the mark bits
        bzero_metadata(&mark_bit_spec, chunk_start, BYTES_IN_CHUNK);

        if chunk_is_empty {
            // Since the chunk mark metadata is a byte, we don't need synchronization
            unsafe { unset_chunk_mark_unsafe(chunk_start) };
        }

        debug!(
            "Used bytes after releasing: {}",
            self.active_bytes.load(Ordering::SeqCst)
        );

        #[cfg(debug_assertions)]
        {
            let completed_packets = self.completed_work_packets.fetch_add(1, Ordering::SeqCst) + 1;
            self.work_live_bytes.fetch_add(live_bytes, Ordering::SeqCst);

            if completed_packets == self.total_work_packets.load(Ordering::Relaxed) {
                trace!(
                    "work_live_bytes = {}, live_bytes = {}, active_bytes = {}",
                    self.work_live_bytes.load(Ordering::Relaxed),
                    live_bytes,
                    self.active_bytes.load(Ordering::Relaxed)
                );

                debug_assert_eq!(
                    self.work_live_bytes.load(Ordering::Relaxed),
                    self.active_bytes.load(Ordering::Relaxed)
                );
            }
        }
    }

    /// This sweep function is called when the mark bit sits in the object header
    ///
    /// This function uses non-atomic accesses to side metadata (although these
    /// non-atomic accesses should not have race conditions associated with them)
    /// as well as calls libc functions (`malloc_usable_size()`, `free()`)
    #[cfg(feature="malloc")]
    fn sweep_chunk_mark_in_header(&self, chunk_start: Address) {
        #[cfg(debug_assertions)]
        let mut live_bytes = 0;

        debug!("Check active chunk {:?}", chunk_start);
        let mut chunk_is_empty = true;
        let mut address = chunk_start;
        let chunk_end = chunk_start + BYTES_IN_CHUNK;
        let mut page = conversions::page_align_down(address);
        let mut page_is_empty = true;
        let mut last_on_page_boundary = false;

        // Linear scan through the chunk
        while address < chunk_end {
            trace!("Check address {}", address);

            if address - page >= BYTES_IN_PAGE {
                if page_is_empty {
                    unsafe { unset_page_mark_unsafe(page) };
                }
                page = conversions::page_align_down(address);
                page_is_empty = !last_on_page_boundary;
                last_on_page_boundary = false;
            }

            // Check if the address is an object
            if unsafe { is_alloced_object_unsafe(address) } {
                let object = unsafe { address.to_object_reference() };
                let obj_start = VM::VMObjectModel::object_start_ref(object);
                let bytes = unsafe { malloc_usable_size(obj_start.to_mut_ptr()) };

                #[cfg(debug_assertions)]
                if ASSERT_ALLOCATION {
                    debug_assert!(
                        self.active_mem.lock().unwrap().contains_key(&obj_start),
                        "Address {} with alloc bit is not in active_mem",
                        obj_start
                    );
                    debug_assert_eq!(
                        self.active_mem.lock().unwrap().get(&obj_start),
                        Some(&bytes),
                        "Address {} size in active_mem does not match the size from malloc_usable_size",
                        obj_start
                    );
                }

                if !is_marked::<VM>(object, None) {
                    // Dead object
                    trace!("Object {} has been allocated but not marked", object);

                    // Free object
                    self.free(obj_start, bytes);
                    trace!("free object {}", object);
                    unsafe { unset_alloc_bit_unsafe(object) };
                } else {
                    // Live object. Unset mark bit
                    unset_mark_bit::<VM>(object, None);
                    // This chunk and page are still active.
                    chunk_is_empty = false;
                    page_is_empty = false;

                    if address + bytes - page > BYTES_IN_PAGE {
                        last_on_page_boundary = true;
                    }

                    #[cfg(debug_assertions)]
                    {
                        // Accumulate live bytes
                        live_bytes += bytes;
                    }
                }

                // Skip to next object
                address += bytes;
            } else {
                // not an object
                address += VM::MIN_ALIGNMENT;
            }
        }

        if chunk_is_empty {
            // Since the chunk mark metadata is a byte, we don't need synchronization
            unsafe { unset_chunk_mark_unsafe(chunk_start) };
        }

        debug!(
            "Used bytes after releasing: {}",
            self.active_bytes.load(Ordering::SeqCst)
        );

        #[cfg(debug_assertions)]
        {
            let completed_packets = self.completed_work_packets.fetch_add(1, Ordering::SeqCst) + 1;
            self.work_live_bytes.fetch_add(live_bytes, Ordering::SeqCst);

            if completed_packets == self.total_work_packets.load(Ordering::Relaxed) {
                trace!(
                    "work_live_bytes = {}, live_bytes = {}, active_bytes = {}",
                    self.work_live_bytes.load(Ordering::Relaxed),
                    live_bytes,
                    self.active_bytes.load(Ordering::Relaxed)
                );
                debug_assert_eq!(
                    self.work_live_bytes.load(Ordering::Relaxed),
                    self.active_bytes.load(Ordering::Relaxed)
                );
            }
        }
    }
}


