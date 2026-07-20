use std::marker::PhantomData;

use crate::{
    plan::{
        tracing::{gc_work::DefaultObjectTracerContext, SlotOfTrace, Trace},
        VectorQueue,
    },
    scheduler::{GCWork, GCWorker, GCWorkerShared, WorkBucketStage, EDGES_WORK_BUFFER_SIZE},
    util::{ObjectReference, VMWorkerThread},
    vm::{slot::Slot, ObjectTracerContext, Scanning, VMBinding},
    MMTK,
};
use std::collections::VecDeque;

/// A work packet for processing slots during a stop-the-world tracing GC and the final mark pause
/// of a concurrent GC.
///
/// It will call `trace_object` on the value of each slot, and updates the slot if the object is
/// moved or forwarded.  It will spawn or immediately run the [`ProcessNodes`] work packet to
/// scan newly traced objects.
pub struct ProcessSlots<T: Trace> {
    slots: VecDeque<SlotOfTrace<T>>,
    pushes: u32,
    bucket: WorkBucketStage,
}

impl<T: Trace> ProcessSlots<T> {
    #[cfg(not(feature = "edge_enqueueing"))]
    const SCAN_OBJECTS_IMMEDIATELY: bool = true;

    pub fn new(slots: VecDeque<SlotOfTrace<T>>, bucket: WorkBucketStage) -> Self {
        Self {
            slots,
            pushes: 0,
            bucket,
        }
    }

    #[cfg(not(feature = "edge_enqueueing"))]
    fn process_slots(
        &mut self,
        worker: &mut GCWorker<T::VM>,
        trace: T,
    ) -> VectorQueue<ObjectReference> {
        let mut queue = VectorObjectQueue::new();

        for slot in self.slots.iter() {
            if let Some(object) = slot.load() {
                let new_object = trace.trace_object(worker, object, &mut queue);
                if T::may_move_objects() && new_object != object {
                    slot.store(new_object);
                }
            }
        }

        queue
    }

    #[cfg(feature = "edge_enqueueing")]
    fn process_slots(&mut self, worker: &mut GCWorker<T::VM>, trace: T) {
        let tls = worker.tls;

        while let Some(slot) = self.slots.pop_front() {
            if let Some(pf_slot) = self.slots.get(31) {
                pf_slot.prefetch_load();
            }
            if let Some(object) = slot.load() {
                let new_object = trace.trace_object(worker, object, &mut |enqueued_object| {
                    debug_assert!(
                        <T::VM as VMBinding>::VMScanning::support_slot_enqueuing(
                            tls,
                            enqueued_object
                        ),
                        "Object {enqueued_object} does not support slot enqueuing."
                    );
                    let mut closure = |slot: SlotOfTrace<T>| {
                        self.slots.push_back(slot);
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

        #[cfg(feature = "extreme_assertions")]
        if crate::util::slot_logger::should_check_duplicate_slots(mmtk.get_plan()) {
            for slot in self.slots.iter() {
                // log slot, panic if already logged
                mmtk.slot_logger.log_slot(*slot);
            }
        }

        #[cfg(feature = "edge_enqueueing")]
        self.process_slots(worker, trace);
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
                // If an object supports slot-enqueuing, we enqueue its slots.
                <T::VM as VMBinding>::VMScanning::scan_object(tls, object, &mut |slot| {
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
