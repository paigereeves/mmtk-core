#[cfg(feature = "mark_queue")]
use std::collections::VecDeque;
use std::marker::PhantomData;

use crate::{
    mmtk::{NULL_BUCKET, NUM_BUCKETS, OB_BUCKET},
    plan::{
        tracing::{gc_work::DefaultObjectTracerContext, SlotOfTrace, Trace},
        VectorQueue,
    },
    scheduler::{GCWork, GCWorker, GCWorkerShared, WorkBucketStage, EDGES_WORK_BUFFER_SIZE},
    util::{ObjectReference, VMWorkerThread},
    vm::{slot::Slot, ObjectTracerContext, Scanning, VMBinding},
    MMTK,
};
use std::sync::Mutex;

/// The type of `object` as a Java-style name (`java.lang.String`, `[B`), from the VM's type
/// descriptor.
#[cfg(any(feature = "histogram_aux", feature = "histogram_real"))]
fn type_name<VM: VMBinding>(object: ObjectReference) -> String {
    use crate::vm::ObjectModel;
    crate::mmtk::type_descriptor_to_name(VM::VMObjectModel::get_type_descriptor(object))
}

/// Per-packet slot type counts, or `None` if the auxiliary trace is not running. Checked once
/// when the packet starts: no packet runs across the `AfterPrepare` or `AfterAuxClosure`
/// sentinels, so a packet is either wholly inside the auxiliary trace or wholly outside it.
#[cfg(feature = "slot_types")]
fn new_slot_type_counts<VM: VMBinding>(mmtk: &MMTK<VM>) -> Option<crate::mmtk::SlotTypeCounts> {
    mmtk.slot_types_active()
        .then(crate::mmtk::SlotTypeCounts::new)
}

/// Count one slot load in `counts`, keyed by the type of the loaded object (`None` for null).
#[cfg(feature = "slot_types")]
fn count_slot_type<VM: VMBinding>(
    counts: &mut Option<crate::mmtk::SlotTypeCounts>,
    obj: Option<ObjectReference>,
) {
    use crate::vm::ObjectModel;
    if let Some(counts) = counts {
        let ty = obj.map(VM::VMObjectModel::get_type_descriptor);
        *counts.entry(ty).or_insert(0) += 1;
    }
}

/// Per-packet slot holder counts, or `None` if the auxiliary trace is not running. See
/// `new_slot_type_counts`.
#[cfg(feature = "slot_types")]
fn new_slot_holder_counts<VM: VMBinding>(mmtk: &MMTK<VM>) -> Option<crate::mmtk::SlotHolderCounts> {
    mmtk.slot_types_active()
        .then(crate::mmtk::SlotHolderCounts::new)
}

/// If `slot` of the scanned object `holder` points to a watched type (`HOLDER_TARGET_TYPES`),
/// count it in `counts` under (target type, holder type).
#[cfg(feature = "slot_types")]
fn count_slot_holder<VM: VMBinding>(
    counts: &mut Option<crate::mmtk::SlotHolderCounts>,
    holder: ObjectReference,
    slot: VM::VMSlot,
) {
    use crate::vm::ObjectModel;
    if let Some(counts) = counts {
        if let Some(target) = slot.load() {
            let target_ty = VM::VMObjectModel::get_type_descriptor(target);
            if crate::mmtk::is_holder_target(target_ty) {
                let holder_ty = VM::VMObjectModel::get_type_descriptor(holder);
                *counts.entry((target_ty, holder_ty)).or_insert(0) += 1;
            }
        }
    }
}

/// Log the object about to be scanned, in the same format as the G1 `gc+marking=trace`
/// "Scanning object" log.
#[cfg(any(feature = "histogram_aux", feature = "histogram_real"))]
fn log_scanning_object<VM: VMBinding>(object: ObjectReference) {
    if log_enabled!(log::Level::Trace) {
        trace!(
            "Scanning object {:#018x} type {}",
            object.to_object_start::<VM>().as_usize(),
            type_name::<VM>(object)
        );
    }
}

pub struct AfterVMRefClosure<T: Trace> {
    phantom_data: PhantomData<T>,
}

impl<T: Trace> GCWork<T::VM> for AfterVMRefClosure<T> {
    fn do_work(&mut self, worker: &mut GCWorker<T::VM>, mmtk: &'static MMTK<T::VM>) {
        let stage = WorkBucketStage::VMRefClosure;

        let need_to_repeat = {
            let tracer_factory = DefaultObjectTracerContext::<T>::new(stage);
            <T::VM as VMBinding>::VMScanning::process_weak_refs(worker, tracer_factory)
        };

        if need_to_repeat {
            // Schedule Self as the new sentinel so we'll call `process_weak_refs` again after the
            // current transitive closure.
            let new_self = Box::new(Self::new());

            worker.scheduler().work_buckets[stage].set_sentinel(new_self);
            return;
        }

        #[cfg(feature = "histogram_real")]
        if !mmtk.get_plan().is_nursery_gc() {
            mmtk.print_histograms();
        }
        #[cfg(feature = "perf_closure")]
        mmtk.stats.perf_ctrl_disable();
    }
}

impl<T: Trace> AfterVMRefClosure<T> {
    pub fn new() -> Self {
        Self {
            phantom_data: PhantomData,
        }
    }
}

pub struct AfterPrepare {}

impl<VM: VMBinding> GCWork<VM> for AfterPrepare {
    fn do_work(&mut self, _worker: &mut GCWorker<VM>, mmtk: &'static MMTK<VM>) {
        #[cfg(any(feature = "histogram_aux", feature = "histogram_real"))]
        mmtk.clear_histograms();
        #[cfg(feature = "slot_types")]
        mmtk.start_slot_types();
        #[cfg(feature = "perf_aux_closure")]
        if !crate::plan::is_nursery_gc(mmtk.get_plan()) {
            mmtk.stats.perf_ctrl_enable();
        }
        #[cfg(all(not(feature = "aux_trace"), feature = "perf_closure"))]
        if !crate::plan::is_nursery_gc(mmtk.get_plan()) {
            mmtk.stats.perf_ctrl_enable();
        }
        #[cfg(feature = "aux_trace")]
        mmtk.reference_processors.set_allow_new_candidates(false);
    }
}

impl AfterPrepare {
    pub fn new() -> Self {
        Self {}
    }
}

#[cfg(feature = "aux_trace")]
pub struct AfterAuxClosure {}

#[cfg(feature = "aux_trace")]
impl<VM: VMBinding> GCWork<VM> for AfterAuxClosure {
    fn do_work(&mut self, _worker: &mut GCWorker<VM>, mmtk: &'static MMTK<VM>) {
        #[cfg(feature = "histogram_aux")]
        if !crate::plan::is_nursery_gc(mmtk.get_plan()) {
            mmtk.print_histograms();
        }
        #[cfg(feature = "histogram_real")]
        mmtk.clear_histograms();
        #[cfg(feature = "slot_types")]
        {
            mmtk.stop_slot_types();
            mmtk.print_and_reset_slot_types();
        }
        #[cfg(feature = "perf_aux_closure")]
        mmtk.stats.perf_ctrl_disable();

        #[cfg(feature = "perf_closure")]
        if !crate::plan::is_nursery_gc(mmtk.get_plan()) {
            mmtk.stats.perf_ctrl_enable();
        }

        #[cfg(feature = "aux_trace")]
        mmtk.reference_processors.set_allow_new_candidates(true);
    }
}

#[cfg(feature = "aux_trace")]
impl AfterAuxClosure {
    pub fn new() -> Self {
        Self {}
    }
}

/// A work packet for processing slots during a stop-the-world tracing GC and the final mark pause
/// of a concurrent GC.
///
/// It will call `trace_object` on the value of each slot, and updates the slot if the object is
/// moved or forwarded.  It will spawn or immediately run the [`ProcessNodes`] work packet to
/// scan newly traced objects.
pub struct ProcessSlots<T: Trace> {
    #[cfg(not(feature = "mark_queue"))]
    slots: Vec<SlotOfTrace<T>>,
    #[cfg(feature = "mark_queue")]
    slots: VecDeque<SlotOfTrace<T>>,
    pushes: u32,
    bucket: WorkBucketStage,
    /// If this packet was created from roots, the kind of root (see
    /// `RootsWorkFactory::set_root_label`). Packets split off later have no label.
    #[cfg(feature = "slot_types")]
    root_label: Option<&'static str>,
}

impl<T: Trace> ProcessSlots<T> {
    #[cfg(not(feature = "edge_enqueueing"))]
    const SCAN_OBJECTS_IMMEDIATELY: bool = true;

    pub fn new(
        #[cfg(not(feature = "mark_queue"))] slots: Vec<SlotOfTrace<T>>,
        #[cfg(feature = "mark_queue")] slots: VecDeque<SlotOfTrace<T>>,
        bucket: WorkBucketStage,
    ) -> Self {
        Self {
            slots,
            pushes: 0,
            bucket,
            #[cfg(feature = "slot_types")]
            root_label: None,
        }
    }

    /// Mark this packet's slots as roots of the kind `label`.
    #[cfg(feature = "slot_types")]
    pub(crate) fn set_root_label(&mut self, label: &'static str) {
        self.root_label = Some(label);
    }

    /// If this is a root packet, count its slots that point to watched types with the root label
    /// as the holder. Must run before tracing starts, while `self.slots` still holds
    /// only the root slots.
    #[cfg(feature = "slot_types")]
    fn count_root_holders(&self, mmtk: &MMTK<T::VM>) {
        use crate::vm::ObjectModel;
        let Some(label) = self.root_label else {
            return;
        };
        let Some(mut counts) = new_slot_holder_counts(mmtk) else {
            return;
        };
        let holder = crate::mmtk::root_label_descriptor(label);
        for slot in self.slots.iter() {
            if let Some(target) = slot.load() {
                let target_ty = <T::VM as VMBinding>::VMObjectModel::get_type_descriptor(target);
                if crate::mmtk::is_holder_target(target_ty) {
                    *counts.entry((target_ty, holder)).or_insert(0) += 1;
                }
            }
        }
        if !counts.is_empty() {
            mmtk.record_slot_holders(counts);
        }
    }

    #[cfg(not(feature = "edge_enqueueing"))]
    fn process_slots(
        &mut self,
        worker: &mut GCWorker<T::VM>,
        trace: T,
    ) -> VectorQueue<ObjectReference> {
        let mut queue = VectorObjectQueue::new();
        #[cfg(feature = "slot_types")]
        let mut slot_types = new_slot_type_counts(worker.mmtk);

        for slot in self.slots.iter() {
            let obj = slot.load();
            #[cfg(feature = "slot_types")]
            count_slot_type::<T::VM>(&mut slot_types, obj);
            if let Some(object) = obj {
                let new_object = trace.trace_object(worker, object, &mut queue);
                if T::may_move_objects() && new_object != object {
                    slot.store(new_object);
                }
            }
        }

        #[cfg(feature = "slot_types")]
        if let Some(slot_types) = slot_types {
            worker.mmtk.record_slot_types(slot_types);
        }
        queue
    }

    #[cfg(not(feature = "mark_queue"))]
    fn pop_slot(&mut self) -> Option<SlotOfTrace<T>> {
        self.slots.pop()
    }
    #[cfg(not(feature = "mark_queue"))]
    fn push_slot(&mut self, slot: SlotOfTrace<T>) {
        self.slots.push(slot);
    }

    #[cfg(feature = "mark_queue")]
    fn pop_slot(&mut self) -> Option<SlotOfTrace<T>> {
        self.slots.pop_front()
    }
    #[cfg(feature = "mark_queue")]
    fn push_slot(&mut self, slot: SlotOfTrace<T>) {
        self.slots.push_back(slot);
    }

    /// Add one to the bucket of `dist_hist` for the log2 distance from `slot` to `obj`, and
    /// return that bucket.
    fn record_ref_dist(
        &mut self,
        slot: <<T as Trace>::VM as VMBinding>::VMSlot,
        obj: Option<ObjectReference>,
        dist_hist: &Mutex<[u64; NUM_BUCKETS]>,
    ) -> usize {
        let mut dist_hist = dist_hist.lock().unwrap();
        let bucket;
        if let Some(obj) = obj {
            let obj_addr = obj.to_object_start::<<T as Trace>::VM>().as_usize() as i128;
            let slot_addr = slot.addr().as_usize() as i128;
            assert!(obj_addr != slot_addr);

            if obj_addr > slot_addr {
                // slot points forward
                let dist = obj_addr - slot_addr;
                let log = dist.ilog2();
                if log > 34 {
                    // out of bounds
                    bucket = OB_BUCKET;
                } else {
                    bucket = log as usize + 35;
                }
            } else {
                let dist = slot_addr - obj_addr;
                let log = dist.ilog2();
                if log > 34 {
                    // out of bounds
                    bucket = OB_BUCKET;
                } else {
                    bucket = 34 - log as usize;
                }
            }
        } else {
            bucket = NULL_BUCKET;
        }
        dist_hist[bucket] += 1;
        bucket
    }

    /// Log one recorded slot, in the same format as the G1 `gc+marking=trace` "Ref dist" log.
    #[cfg(any(feature = "histogram_aux", feature = "histogram_real"))]
    fn log_ref_dist(
        slot: <<T as Trace>::VM as VMBinding>::VMSlot,
        obj: Option<ObjectReference>,
        bucket: usize,
        trace: &T,
    ) {
        if !log_enabled!(log::Level::Trace) {
            return;
        }
        let slot_addr = slot.addr().as_usize();
        let (obj_addr, type_name, distance, marked) = match obj {
            Some(o) => {
                let obj_addr = o.to_object_start::<T::VM>().as_usize();
                #[cfg(feature = "histogram_aux")]
                let marked = trace.obj_is_aux_marked(o);
                #[cfg(not(feature = "histogram_aux"))]
                let marked = trace.obj_is_marked(o);
                (
                    obj_addr,
                    type_name::<T::VM>(o),
                    obj_addr as i128 - slot_addr as i128,
                    marked,
                )
            }
            None => (0, "null".to_string(), 0, false),
        };
        trace!(
            "Ref dist: slot {:#018x} -> {:#018x} type {} distance {} bucket {} marked {}",
            slot_addr,
            obj_addr,
            type_name,
            distance,
            bucket,
            marked
        );
    }

    #[cfg(feature = "edge_enqueueing")]
    fn process_slots(
        &mut self,
        worker: &mut GCWorker<T::VM>,
        trace: T,
        all_slots_histogram: &Mutex<[u64; NUM_BUCKETS]>,
        unmarked_slots_histogram: &Mutex<[u64; NUM_BUCKETS]>,
    ) {
        let tls = worker.tls;
        #[cfg(feature = "slot_types")]
        let mut slot_types = new_slot_type_counts(worker.mmtk);
        #[cfg(feature = "slot_types")]
        let mut slot_holders = new_slot_holder_counts(worker.mmtk);
        while let Some(slot) = self.pop_slot() {
            let obj = slot.load();
            #[cfg(feature = "slot_types")]
            count_slot_type::<T::VM>(&mut slot_types, obj);
            #[cfg(any(feature = "histogram_aux", feature = "histogram_real"))]
            {
                let bucket = self.record_ref_dist(slot, obj, all_slots_histogram);
                Self::log_ref_dist(slot, obj, bucket, &trace);
            }
            if let Some(object) = obj {
                #[cfg(feature = "histogram_aux")]
                if !trace.obj_is_aux_marked(object) {
                    self.record_ref_dist(slot, obj, unmarked_slots_histogram);
                }
                #[cfg(feature = "histogram_real")]
                if !trace.obj_is_marked(object) {
                    self.record_ref_dist(slot, obj, unmarked_slots_histogram);
                }
                let new_object = trace.trace_object(worker, object, &mut |enqueued_object| {
                    debug_assert!(
                        <T::VM as VMBinding>::VMScanning::support_slot_enqueuing(
                            tls,
                            enqueued_object
                        ),
                        "Object {enqueued_object} does not support slot enqueuing."
                    );
                    #[cfg(any(feature = "histogram_aux", feature = "histogram_real"))]
                    log_scanning_object::<T::VM>(enqueued_object);
                    let mut closure = |slot: SlotOfTrace<T>| {
                        #[cfg(feature = "slot_types")]
                        count_slot_holder::<T::VM>(&mut slot_holders, enqueued_object, slot);
                        self.push_slot(slot);
                        self.pushes += 1;
                    };
                    <T::VM as VMBinding>::VMScanning::scan_object(
                        tls,
                        enqueued_object,
                        &mut closure,
                    );
                    trace.post_scan_object(enqueued_object);
                });
                if self.slots.len() >= EDGES_WORK_BUFFER_SIZE
                    || self.pushes >= (EDGES_WORK_BUFFER_SIZE / 2) as u32
                {
                    self.flush_half(worker);
                }

                if T::may_move_objects() && new_object != object {
                    slot.store(new_object);
                }
            }
        }
        #[cfg(feature = "slot_types")]
        if let Some(slot_types) = slot_types {
            worker.mmtk.record_slot_types(slot_types);
        }
        #[cfg(feature = "slot_types")]
        if let Some(slot_holders) = slot_holders {
            worker.mmtk.record_slot_holders(slot_holders);
        }
    }

    #[cfg(not(feature = "edge_enqueueing"))]
    fn flush(&mut self, worker: &mut GCWorker<T::VM>, mut queue: VectorQueue<ObjectReference>) {
        if queue.is_empty() {
            return;
        }

        let queued_objects = queue.take();
        let mut work = ProcessNodes::<T>::new(queued_objects, self.bucket);

        if Self::SCAN_OBJECTS_IMMEDIATELY {
            work.do_work(worker, worker.mmtk);
        } else {
            worker.add_work(self.bucket, work);
        }
    }

    #[cfg(feature = "edge_enqueueing")]
    fn flush_half(&mut self, worker: &mut GCWorker<T::VM>) {
        let slots = if self.slots.len() > 1 {
            let half = self.slots.len() / 2;
            self.slots.split_off(half)
        } else {
            return;
        };

        self.pushes = self.slots.len() as u32;
        if slots.is_empty() {
            return;
        }

        let w = Self::new(slots, self.bucket);
        worker.add_work(self.bucket, w);
    }
}

impl<T: Trace> GCWork<T::VM> for ProcessSlots<T> {
    fn do_work(&mut self, worker: &mut GCWorker<T::VM>, mmtk: &'static MMTK<T::VM>) {
        probe!(mmtk, process_slots, self.slots.len());

        let trace = T::from_mmtk(mmtk);

        #[cfg(feature = "slot_types")]
        self.count_root_holders(mmtk);

        #[cfg(feature = "extreme_assertions")]
        if crate::util::slot_logger::should_check_duplicate_slots(mmtk.get_plan()) {
            for slot in self.slots.iter() {
                // log slot, panic if already logged
                mmtk.slot_logger.log_slot(*slot);
            }
        }

        #[cfg(feature = "edge_enqueueing")]
        self.process_slots(
            worker,
            trace,
            &mmtk.all_slots_histogram,
            &mmtk.unmarked_slots_histogram,
        );
        #[cfg(not(feature = "edge_enqueueing"))]
        {
            let queue = self.process_slots(worker, trace);

            self.flush(worker, queue);
        }
    }
}

/// A work packet for scanning objects and optionally do node-enqueuing tracing during a
/// stop-the-world tracing GC and the final mark pause of a concurrent GC.
///
/// It will scan each objects.  For objects that supports slot enqueuing, it will collect their
/// slots and spawn [`ProcessSlots`] work packets to trace them.  For objects that don't
/// support slot enqueuing, it will immediately trace their slots and spawn other
/// [`ProcessNodes`] work packets to process their newly traced children.
pub struct ProcessNodes<T: Trace> {
    objects: Vec<ObjectReference>,
    bucket: WorkBucketStage,
    phantom_data: PhantomData<T>,
}

impl<T: Trace> ProcessNodes<T> {
    pub fn new(objects: Vec<ObjectReference>, bucket: WorkBucketStage) -> Self {
        Self {
            objects,
            bucket,
            phantom_data: PhantomData,
        }
    }

    fn try_enqueue_slots(
        &mut self,
        worker: &mut GCWorker<T::VM>,
        tls: VMWorkerThread,
        trace: &T,
    ) -> Vec<ObjectReference> {
        // We record objects that don't support slot-enqueuing tracing and process them later.
        let mut scan_later = Vec::new();

        let mut slots = VectorQueue::new();
        #[cfg(feature = "slot_types")]
        let mut slot_holders = new_slot_holder_counts(worker.mmtk);

        let flush = |slots: &mut VectorQueue<_>, worker: &mut GCWorker<T::VM>| {
            let buffer = slots.take();
            let work_packet = ProcessSlots::<T>::new(buffer.into(), self.bucket);
            worker.add_work(self.bucket, work_packet);
        };

        // For any object we need to scan, we count its live bytes.
        // Check the option outside the loop for better performance.
        //
        // TODO: Currently all objects reached in a GC will be processed here,
        // so it is a good place to do statistics for all reachable objects.
        // In the future, when we refactor the ProcessNodes and ProcessSlots work packets
        // so that each of them can compute the transitive closure alone (i.e. removing double enqueuing),
        // we need to make sure both work packets will count the live bytes.
        if crate::util::rust_util::unlikely(*worker.mmtk.get_options().count_live_bytes_in_gc) {
            // Borrow before the loop.
            let mut live_bytes_stats = worker.shared.live_bytes_per_space.borrow_mut();
            for object in self.objects.iter().copied() {
                GCWorkerShared::<T::VM>::increase_live_bytes(&mut live_bytes_stats, object);
            }
        }

        for object in self.objects.iter().copied() {
            if <T::VM as VMBinding>::VMScanning::support_slot_enqueuing(tls, object) {
                trace!("Scan object (slot) {}", object);
                #[cfg(any(feature = "histogram_aux", feature = "histogram_real"))]
                log_scanning_object::<T::VM>(object);
                // If an object supports slot-enqueuing, we enqueue its slots.
                <T::VM as VMBinding>::VMScanning::scan_object(tls, object, &mut |slot| {
                    #[cfg(feature = "slot_types")]
                    count_slot_holder::<T::VM>(&mut slot_holders, object, slot);
                    slots.push(slot);
                    if slots.is_full() {
                        flush(&mut slots, worker);
                    }
                });
                trace.post_scan_object(object);
            } else {
                // If an object does not support slot-enqueuing, we have to use
                // `Scanning::scan_object_and_trace_edges` and offload the job of updating the
                // reference field to the VM.
                //
                // TODO: We may refactor this work packet to do slot-enqueuing and node-enqueuing in
                // one loop.
                scan_later.push(object);
            }
        }

        if !slots.is_empty() {
            flush(&mut slots, worker);
        }

        #[cfg(feature = "slot_types")]
        if let Some(slot_holders) = slot_holders {
            worker.mmtk.record_slot_holders(slot_holders);
        }

        scan_later
    }

    fn do_node_enqueuing_tracing(
        &mut self,
        worker: &mut GCWorker<T::VM>,
        tls: VMWorkerThread,
        trace: T,
        scan_later: Vec<ObjectReference>,
    ) {
        if scan_later.is_empty() {
            return;
        }

        let object_tracer_context = DefaultObjectTracerContext::<T>::new(self.bucket);

        object_tracer_context.with_tracer(worker, |object_tracer| {
            // Scan objects and trace their outgoing edges at the same time.
            for object in scan_later.iter().copied() {
                trace!("Scan object (node) {}", object);
                <T::VM as VMBinding>::VMScanning::scan_object_and_trace_edges(
                    tls,
                    object,
                    object_tracer,
                );
                trace.post_scan_object(object);
            }
        });
    }
}

impl<T: Trace> GCWork<T::VM> for ProcessNodes<T> {
    fn do_work(&mut self, worker: &mut GCWorker<T::VM>, mmtk: &'static MMTK<T::VM>) {
        trace!("ScanObjects");

        let tls = worker.tls;
        let trace = T::from_mmtk(mmtk);

        // Go through the object list and scan objects that supports slot-enququing.
        let scan_later = self.try_enqueue_slots(worker, tls, &trace);

        let total_objects = self.objects.len();
        let scan_and_trace = scan_later.len();
        probe!(mmtk, process_nodes, total_objects, scan_and_trace);

        // If any objects do not support slot-enqueuing, we process them now.
        self.do_node_enqueuing_tracing(worker, tls, trace, scan_later);

        trace!("ScanObjects End");
    }
}
