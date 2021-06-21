use std::{collections::{HashMap, LinkedList}, vec};

use crate::{Plan, policy::marksweepspace::MarkSweepSpace, util::{Address, VMThread}, vm::VMBinding};

use super::Allocator;

pub struct FreeListAllocator<VM: VMBinding> {
    pub tls: VMThread,
    space: &'static MarkSweepSpace<VM>,
    plan: &'static dyn Plan<VM = VM>,
    available_blocks: HashMap<SizeClass, LinkedList<Block>>,
    exhausted_blocks: HashMap<SizeClass, LinkedList<Block>>,
    free_lists: HashMap<Block, Vec<Block>>,
  }
  
  type SizeClass = usize;
  type Block = Address;

// struct Blocks {
//     available_blocks: HashMap<SizeClass, LinkedList<Block>>,
//     exhausted_blocks: HashMap<SizeClass, LinkedList<Block>>,
//     free_lists: HashMap<Block, Vec<Block>>,
// }

impl<VM: VMBinding> Allocator<VM> for FreeListAllocator<VM> {
    fn get_tls(&self) -> VMThread {
        todo!()
    }

    fn get_space(&self) -> &'static dyn crate::policy::space::Space<VM> {
        self.space
    }

    fn get_plan(&self) -> &'static dyn Plan<VM = VM> {
        todo!()
    }

    fn alloc(&mut self, size: usize, align: usize, offset: isize) -> crate::util::Address {
        unsafe { Address::zero() }
    }

    fn alloc_slow_once(&mut self, size: usize, align: usize, offset: isize) -> crate::util::Address {
        todo!()
    }
}

impl<VM: VMBinding> FreeListAllocator<VM> {
    pub fn new(
        tls: VMThread,
        space: &'static MarkSweepSpace<VM>,
        plan: &'static dyn Plan<VM = VM>,
    ) -> Self {
        let mut allocator = FreeListAllocator {
            tls,
            space,
            plan,
            available_blocks: HashMap::new(),
            exhausted_blocks: HashMap::new(),
            free_lists: HashMap::new(),
        };
        allocator
    }
}