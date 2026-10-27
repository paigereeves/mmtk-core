use super::MarkSweep;
use crate::plan::tracing::PlanTrace;
#[cfg(not(feature = "aux_trace"))]
use crate::plan::tracing::UnsupportedTrace;
#[cfg(feature = "aux_trace")]
use crate::policy::gc_work::TRACE_KIND_AUX;
use crate::policy::gc_work::DEFAULT_TRACE;
use crate::vm::VMBinding;

pub struct MSGCWorkContext<VM: VMBinding>(std::marker::PhantomData<VM>);
impl<VM: VMBinding> crate::scheduler::GCWorkContext for MSGCWorkContext<VM> {
    type VM = VM;
    type PlanType = MarkSweep<VM>;
    type DefaultTrace = PlanTrace<MarkSweep<VM>, DEFAULT_TRACE>;
    type PinningTrace = PlanTrace<MarkSweep<VM>, DEFAULT_TRACE>;
    #[cfg(feature = "aux_trace")]
    type AuxiliaryTrace = PlanTrace<MarkSweep<VM>, TRACE_KIND_AUX>;
    #[cfg(not(feature = "aux_trace"))]
    type AuxiliaryTrace = UnsupportedTrace<VM>;
}
