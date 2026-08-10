use super::global::SemiSpace;
use crate::plan::tracing::{PlanTrace, UnsupportedTrace};
use crate::policy::gc_work::DEFAULT_TRACE;
use crate::policy::gc_work::TRACE_KIND_AUX;
use crate::vm::VMBinding;

/// Auxiliary trace
pub type AuxiliaryTrace<VM> = PlanTrace<SemiSpace<VM>, TRACE_KIND_AUX>;

pub struct SSGCWorkContext<VM: VMBinding>(std::marker::PhantomData<VM>);
impl<VM: VMBinding> crate::scheduler::GCWorkContext for SSGCWorkContext<VM> {
    type VM = VM;
    type PlanType = SemiSpace<VM>;
    type DefaultTrace = PlanTrace<SemiSpace<VM>, DEFAULT_TRACE>;
    type PinningTrace = UnsupportedTrace<VM>;
    type AuxiliaryTrace = AuxiliaryTrace<VM>;
}
