use super::global::SemiSpace;
use crate::plan::tracing::{PlanTrace, UnsupportedTrace};
use crate::policy::gc_work::TraceKind;
use crate::vm::VMBinding;

pub struct SSGCWorkContext<VM: VMBinding, const KIND: TraceKind>(std::marker::PhantomData<VM>);
impl<VM: VMBinding, const KIND: TraceKind> crate::scheduler::GCWorkContext
    for SSGCWorkContext<VM, KIND>
{
    type VM = VM;
    type PlanType = SemiSpace<VM>;
    #[cfg(feature = "single_worker")]
    type STPlanType = SemiSpace<VM>;
    type DefaultTrace = PlanTrace<SemiSpace<VM>, KIND>;
    type PinningTrace = UnsupportedTrace<VM>;
}
