// Copyright (c) 2021 Quark Container Authors
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.
//

use core::alloc::{GlobalAlloc, Layout};
use core::arch::asm;
use core::sync::atomic::AtomicBool;
use core::sync::atomic::AtomicU64;
use core::sync::atomic::Ordering;

use crate::qlib::fileinfo::*;

use self::kernel::socket::hostinet::tsot_mgr::TsotSocketMgr;
use self::tsot_msg::TsotMessage;
use self::kernel::dns::dns_svc::DnsSvc;
use super::qlib::kernel::asm::*;
use super::qlib::kernel::quring::uring_async::UringAsyncMgr;
use super::qlib::kernel::taskMgr::*;
use super::qlib::kernel::threadmgr::task_sched::*;
use super::qlib::kernel::vcpu::*;
use super::qlib::kernel::SHARESPACE;
use super::qlib::kernel::TSC;

use super::qlib::common::*;
use super::qlib::kernel::memmgr::pma::*;
use super::qlib::kernel::task::*;
use super::qlib::kernel::taskMgr;
use super::qlib::linux_def::*;
use super::qlib::loader::*;
use super::qlib::mem::bitmap_allocator::*;
use super::qlib::mem::list_allocator::*;
use super::qlib::mutex::*;
use super::qlib::perf_tunning::*;
use super::qlib::qmsg::*;
use super::qlib::task_mgr::*;
use super::qlib::vcpu_mgr::*;
use super::qlib::ShareSpace;
use super::qlib::*;
use super::syscalls::sys_file::*;
use super::Kernel::HostSpace;


use crate::GLOBAL_ALLOCATOR;

use crate::PRIVATE_VCPU_ALLOCATOR;
use crate::PRIVATE_VCPU_SHARED_ALLOCATOR;
use super::qlib::qmsg::sharepara::*;
use crate::qlib::kernel::arch::tee::is_cc_active;
use crate::GUEST_HOST_SHARED_ALLOCATOR;
use alloc::boxed::Box;
use crate::qlib::config::CCMode;

impl OOMHandler for ListAllocator {
    fn handleError(&self, size: u64, alignment: u64) {
        HostSpace::KernelOOM(size, alignment);
    }
}

impl ListAllocator {
    pub fn initialize(&self) -> () {
        self.initialized.store(true, Ordering::Relaxed);
    }

    pub fn Check(&self) {
        Task::StackOverflowCheck();
    }
}

impl CPULocal {
    pub fn Wakeup(&self) {
        super::Kernel::HostSpace::EventfdWrite(self.eventfd);
    }
}

impl<'a> ShareSpace {
    pub fn AQCall(&self, msg: &HostOutputMsg) {
        loop {
            match self.QOutput.Push(msg) {
                Ok(()) => {
                    break;
                }
                Err(_) => (),
            };
        }

        if self.HostProcessor() == 0 {
            self.scheduler.VcpuArr[0].Wakeup();
        }
    }

    pub fn Yield() {
        HostSpace::VcpuYield();
    }
}

impl<T: ?Sized> QMutexIntern<T> {
    pub fn GetID() -> u64 {
        return Task::TaskAddress();
    }
}

#[repr(usize)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PerfType {
    Start,
    Kernel,
    User,
    Read,
    Write,
    Open,
    KernelHandling,
    Print,
    Idle,
    PageFault,
    QCall,
    SysCall,
    Blocked,
    HostInputProcess,
    End,
}

impl CounterSet {
    pub const PERM_COUNTER_SET_SIZE: usize = 8;

    pub fn GetPerfId(&self) -> usize {
        CPULocal::CpuId() as usize
    }

    pub fn PerfType(&self) -> &str {
        return "PerfPrint::Kernel";
    }
}

#[inline]
pub fn switch(from: TaskId, to: TaskId) {
    Task::Current().AccountTaskEnter(SchedState::Blocked);
    CPULocal::SetCurrentTask(to.Addr());
    let fromCtx = from.GetTask();
    let toCtx = to.GetTask();

    if !SHARESPACE.config.read().KernelPagetable {
        toCtx.SwitchPageTable();
    }
    toCtx.SetTLS();

    fromCtx.mm.VcpuLeave();
    toCtx.mm.VcpuEnter();

    if !is_cc_active() {
        unsafe {
            context_swap(fromCtx.GetContext(), toCtx.GetContext());
        }
    } else {
        assert!(!HostAllocator::IsSharedHeapAddr(fromCtx as *const _ as u64));
        assert!(!HostAllocator::IsSharedHeapAddr(toCtx as *const _ as u64));
        unsafe {
            context_swap_cc(
                fromCtx.GetContext(),
                toCtx.GetContext(),
                from.GetTaskWrapper() as *const _ as u64,
                to.GetTaskWrapper() as *const _ as u64,
            );
        }
    }

    //Task::Current().PerfGofrom(PerfType::Blocked);
    Task::Current().AccountTaskLeave(SchedState::Blocked);
}

pub fn OpenAt(task: &Task, dirFd: i32, addr: u64, flags: u32) -> Result<i32> {
    return openAt(task, dirFd, addr, flags);
}

pub fn StartRootContainer(para: *const u8) {
    super::StartRootContainer(para)
}

pub fn StartExecProcess(fd: i32, process: Process) {
    super::StartExecProcess(fd, process)
}
pub fn StartSubContainerProcess(elfEntry: u64, userStackAddr: u64, kernelStackAddr: u64) {
    super::StartSubContainerProcess(elfEntry, userStackAddr, kernelStackAddr)
}

extern "C" {
    pub fn CopyPageUnsafe(to: u64, from: u64);
}

#[cfg(target_arch = "x86_64")]
#[inline]
pub fn Invlpg(addr: u64) {
    if !super::SHARESPACE.config.read().KernelPagetable {
        unsafe {
            asm!("
            invlpg [{0}]
            ",
            in(reg) addr)
        };
    }
}

#[cfg(target_arch = "aarch64")]
#[inline]
pub fn Invlpg(addr: u64) {
    if !super::SHARESPACE.config.read().KernelPagetable {
        unsafe {
            asm!("
            dsb ishst
            tlbi vaae1is, {}
            dsb ish
            isb
        ", in(reg) (addr >> MemoryDef::PAGE_SHIFT));
        };
    }
}

#[inline(always)]
fn _hcall_prepare_shared_buff(vcpu_id: usize, arg0: u64, arg1: u64, arg2: u64, arg3: u64) {
    let shared_buff_addr = MemoryDef::HYPERCALL_PARA_PAGE_OFFSET as *mut ShareParaPage;
    let shared_buff = unsafe {
        &mut (*shared_buff_addr).SharePara[vcpu_id]
    };

    shared_buff.para1 = arg0;
    shared_buff.para2 = arg1;
    shared_buff.para3 = arg2;
    shared_buff.para4 = arg3;
}

#[cfg(target_arch = "x86_64")]
#[inline(always)]
pub fn HyperCall64(type_: u16, para1: u64, para2: u64, para3: u64, para4: u64) {
    if crate::qlib::kernel::arch::tee::is_cc_active() == false {
        unsafe {
            let data: u8 = 0;
            asm!("
                out dx, al
                ",
                in("dx") type_,
                in("al") data,
                in("rsi") para1,
                in("rcx") para2,
                in("rdi") para3,
                in("r10") para4
            )
        }
    } else {
        // We can not query which is the current vCpu before the share space is initialized.
        let vcpu_id = if type_ != crate::qlib::HYPERCALL_SHARESPACE_INIT {
            GetVcpuId()
        } else {
             0
        };
        _hcall_prepare_shared_buff(vcpu_id, para1, para2, para3, para4);
        let dummy_data: u8 = 0;
        unsafe {
            asm!("
                out dx, al
                ",
                in("dx") type_,
                in("al") dummy_data,
            )
        }
    }
}

#[cfg(target_arch = "aarch64")]
#[inline(always)]
pub fn HyperCall64(type_: u16, para1: u64, para2: u64, para3: u64, para4: u64) {
    // Use MMIO to cause VM exit hence "hypercall". The data does not matter,
    // addr of the MMIO identifies the Hypercall. x0 and x1 (w1) are used by the
    // str instruction, the 4x 64-bit parameters are passed via x2,x3,x4,x5.
    if crate::qlib::kernel::arch::tee::is_cc_active() == false {
        let data: u8 = 0;
        let addr: u64 = MemoryDef::HYPERCALL_MMIO_BASE + (type_ as u64);
        unsafe {
            asm!("str w1, [x0]",
                in("x0") addr,
                in("w1") data,
                in("x2") para1,
                in("x3") para2,
                in("x4") para3,
                in("x5") para4,
            )
        }
    } else {
        // We can not query which is the current vCpu before the share space is initialized.
        let vcpu_id = if type_ != crate::qlib::HYPERCALL_SHARESPACE_INIT {
            GetVcpuId()
        } else {
             0
        };
        _hcall_prepare_shared_buff(vcpu_id, para1, para2, para3, para4);
        let dummy_data: u8 = 0;
        let hcall_id = MemoryDef::HYPERCALL_MMIO_BASE + type_ as u64;
        unsafe {
            asm!("str w1, [x0]",
                 in("x0") hcall_id,
                 in("w1") dummy_data,
            )
        }
    }
}

impl CPULocal {
    pub fn CpuId() -> usize {
        return GetVcpuId();
    }
}

impl PageMgr {
    pub fn CopyVsysCallPages(&self, addr: u64) {
        CopyPage(addr, __vsyscall_page as u64);
    }
}

pub fn ClockGetTime(clockId: i32) -> i64 {
    return HostSpace::KernelGetTime(clockId).unwrap();
}

pub fn VcpuFreq() -> i64 {
    return HostSpace::KernelVcpuFreq();
}

pub fn NewSocket(fd: i32) -> i64 {
    return HostSpace::NewSocket(fd);
}

impl HostSpace {
    pub fn Close(fd: i32) -> i64 {
        if is_cc_active() {
            let mut msg = Box::new_in(Msg::Close(qcall::Close { fd }), GUEST_HOST_SHARED_ALLOCATOR);

            return HostSpace::HCall(&mut msg, false) as i64;
        } else {
            let mut msg = Msg::Close(qcall::Close { fd });
            return HostSpace::HCall(&mut msg, false) as i64;
        }
    }

    pub fn Call(msg: &mut Msg, _mustAsync: bool) -> u64 {
        if is_cc_active() {
            let current = Task::Current().GetTaskId();

            let qMsg = Box::new_in(
                QMsg {
                    taskId: current,
                    globalLock: true,
                    ret: 0,
                    msg: msg,
                },
                GUEST_HOST_SHARED_ALLOCATOR,
            );

            let addr = &*qMsg as *const _ as u64;
            let om = Box::new_in(HostOutputMsg::QCall(addr), GUEST_HOST_SHARED_ALLOCATOR);
            super::SHARESPACE.AQCall(&*om);
            taskMgr::Wait();
            return qMsg.ret;
        } else {
            let current = Task::Current().GetTaskId();

            let qMsg = QMsg {
                taskId: current,
                globalLock: true,
                ret: 0,
                msg: msg,
            };

            let addr = &qMsg as *const _ as u64;
            let om = HostOutputMsg::QCall(addr);

            super::SHARESPACE.AQCall(&om);
            taskMgr::Wait();
            return qMsg.ret;
        }
    }

    pub fn HCall(msg: &mut Msg, lock: bool) -> u64 {
        if is_cc_active() {
            let taskId = Task::Current().GetTaskId();
            let mut event = Box::new_in(
                QMsg {
                    taskId: taskId,
                    globalLock: lock,
                    ret: 0,
                    msg: msg,
                },
                GUEST_HOST_SHARED_ALLOCATOR,
            );

            HyperCall64(HYPERCALL_HCALL, &mut *event as *const _ as u64, 0, 0, 0);
            return event.ret;
        } else {
            let taskId = Task::Current().GetTaskId();

            let mut event = QMsg {
                taskId: taskId,
                globalLock: lock,
                ret: 0,
                msg: msg,
            };

            HyperCall64(HYPERCALL_HCALL, &mut event as *const _ as u64, 0, 0, 0);
            return event.ret;
        }
    }
}

#[inline]
pub fn child_clone(userSp: u64) {
    let currTask = Task::Current();
    CPULocal::SetUserStack(userSp);
    CPULocal::SetKernelStack(currTask.GetKernelSp());

    currTask.AccountTaskEnter(SchedState::RunningApp);
    let pt = currTask.GetPtRegs();

    let kernelRsp = pt as *const _ as u64;
    CPULocal::Myself().SetEnterAppTimestamp(TSC.Rdtsc());
    CPULocal::Myself().SetMode(VcpuMode::User);
    currTask.mm.HandleTlbShootdown();
    debug!("entering child task: kernelSp/PtRegs @ {:x}", kernelRsp);
    #[cfg(target_arch = "x86_64")]
    SyscallRet(kernelRsp);
    #[cfg(target_arch = "aarch64")]
    IRet(kernelRsp);
}

extern "C" {
    pub fn initX86FPState(data: u64, useXsave: bool);
}

pub fn InitX86FPState(data: u64, useXsave: bool) {
    unsafe { initX86FPState(data, useXsave) }
}

impl BitmapAllocatorWrapper {
    pub const fn New() -> Self {
        return Self {
            addr: AtomicU64::new(MemoryDef::HEAP_OFFSET),
        };
    }

    pub fn Init(&self) {
        self.addr.store(MemoryDef::HEAP_OFFSET, Ordering::SeqCst);
    }
}

impl HostAllocator {
    pub const fn New() -> Self {
        return Self {
            ioHeapAddr: AtomicU64::new(0),
            guestPrivHeapAddr: AtomicU64::new(0),
            hostInitHeapAddr: AtomicU64::new(0),
            sharedHeapAddr: AtomicU64::new(0),
            vmLaunched: AtomicBool::new(true),
            initialized: AtomicBool::new(true),
        };
    }

    pub fn InitPrivateAllocator(&self, mode: CCMode) {
        match mode {
            CCMode::NormalEmu => {
                crate::qlib::kernel::Kernel::IDENTICAL_MAPPING.store(false, Ordering::SeqCst);
                self.guestPrivHeapAddr.store(
                    MemoryDef::GUEST_PRIVATE_RUNNING_HEAP_OFFSET,
                    Ordering::SeqCst,
                );
                *self.GuestPrivateAllocator() = ListAllocator::New(
                    MemoryDef::GUEST_PRIVATE_RUNNING_HEAP_OFFSET,
                    MemoryDef::GUEST_PRIVATE_RUNNING_HEAP_OFFSET
                        + MemoryDef::GUEST_PRIVATE_RUNNING_HEAP_SIZE,
                );
                let size = core::mem::size_of::<ListAllocator>();
                self.GuestPrivateAllocator().Add(
                    MemoryDef::GUEST_PRIVATE_RUNNING_HEAP_OFFSET as usize + size,
                    MemoryDef::GUEST_PRIVATE_RUNNING_HEAP_SIZE as usize - size,
                );
            }
            CCMode::None => {
                self.guestPrivHeapAddr
                    .store(MemoryDef::HEAP_OFFSET, Ordering::SeqCst);
            }
            _ => {
                self.guestPrivHeapAddr
                    .store(MemoryDef::GUEST_PRIVATE_HEAP_OFFSET, Ordering::SeqCst);
            }
        }
    }

    pub fn InitSharedAllocator(&self, mode: CCMode) {
        match mode {
            CCMode::None => self
                .sharedHeapAddr
                .store(MemoryDef::HEAP_OFFSET, Ordering::SeqCst),
            _ => {
                self.sharedHeapAddr
                    .store(MemoryDef::GUEST_HOST_SHARED_HEAP_OFFSET, Ordering::SeqCst);
                let sharedHeapStart = self.sharedHeapAddr.load(Ordering::Relaxed);
                let sharedHeapEnd = sharedHeapStart + MemoryDef::GUEST_HOST_SHARED_HEAP_SIZE as u64;
                *self.GuestHostSharedAllocator() =
                    ListAllocator::New(sharedHeapStart as _, sharedHeapEnd);
                let ioHeapEnd = sharedHeapEnd + MemoryDef::IO_HEAP_SIZE;

                self.ioHeapAddr.store(sharedHeapEnd, Ordering::SeqCst);
                *self.IOAllocator() = ListAllocator::New(sharedHeapEnd as _, ioHeapEnd);

                let size = core::mem::size_of::<ListAllocator>();
                self.IOAllocator().Add(
                    MemoryDef::HEAP_END as usize + size,
                    MemoryDef::IO_HEAP_SIZE as usize - size,
                );
                // reserve 4 pages for the listAllocator and share para page
                let size = 4 * MemoryDef::PAGE_SIZE as usize;
                self.GuestHostSharedAllocator().Add(
                    MemoryDef::GUEST_HOST_SHARED_HEAP_OFFSET as usize + size,
                    MemoryDef::GUEST_HOST_SHARED_HEAP_SIZE as usize - size,
                );
            }
        };
    }
}

unsafe impl GlobalAlloc for HostAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        return self.GuestPrivateAllocator().alloc(layout);
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        let addr = ptr as u64;
        if !is_cc_active() {
            if Self::IsIOBuf(addr) {
                self.IOAllocator().dealloc(ptr, layout);
            } else {
                self.GuestHostSharedAllocator().dealloc(ptr, layout);
            }
        } else {
            if self.IsGuestPrivateHeapAddr(addr) {
                self.GuestPrivateAllocator().dealloc(ptr, layout);
            } else if Self::IsSharedHeapAddr(addr) {
                self.GuestHostSharedAllocator().dealloc(ptr, layout);
            } else if Self::IsIOBuf(addr) {
                self.IOAllocator().dealloc(ptr, layout);
            }
        }
    }
}

#[inline]
pub fn VcpuId() -> usize {
    return CPULocal::CpuId();
}

pub fn HugepageDontNeed(addr: u64) {
    let ret = HostSpace::Madvise(
        addr,
        MemoryDef::HUGE_PAGE_SIZE as usize,
        MAdviseOp::MADV_DONTNEED,
    );
    assert!(ret == 0, "HugepageDontNeed fail with {}", ret)
}

impl IOMgr {
    pub fn Init() -> Result<Self> {
        return Err(Error::Common(format!("IOMgr can't init in kernel")));
    }
}

unsafe impl GlobalAlloc for GlobalVcpuAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        if !self.init.load(Ordering::Acquire) {
            return GLOBAL_ALLOCATOR.alloc(layout);
        }
        if is_cc_active(){
            return PRIVATE_VCPU_ALLOCATOR.AllocatorMut().alloc(layout);
        } else {
            return CPU_LOCAL[VcpuId()].AllocatorMut().alloc(layout);
        }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        if !HostAllocator::IsHeapAddr(ptr as u64) {
            return GLOBAL_ALLOCATOR.dealloc(ptr, layout);
        }

        if !self.init.load(Ordering::Relaxed) {
            return GLOBAL_ALLOCATOR.dealloc(ptr, layout);
        }
        if is_cc_active(){
            return PRIVATE_VCPU_ALLOCATOR.AllocatorMut().dealloc(ptr, layout);
        } else {
            return CPU_LOCAL[VcpuId()].AllocatorMut().dealloc(ptr, layout);
        }
    }
}

use alloc::alloc::AllocError;
use core::ptr::NonNull;

unsafe impl core::alloc::Allocator for GuestHostSharedAllocator {
    fn allocate(&self, layout: Layout) -> core::result::Result<NonNull<[u8]>, AllocError> {
        unsafe{
            if !GUEST_HOST_SHARED_ALLOCATOR_INIT.load(Ordering::Acquire) {
                let ptr = GLOBAL_ALLOCATOR.AllocSharedBuf(layout.size(), layout.align());
                let slice = core::slice::from_raw_parts_mut(ptr, layout.size());
                return Ok(NonNull::new_unchecked(slice));
            }
            if is_cc_active(){
                let ptr = PRIVATE_VCPU_SHARED_ALLOCATOR.AllocatorMut().alloc(layout);
                let slice = core::slice::from_raw_parts_mut(ptr, layout.size());
                return Ok(NonNull::new_unchecked(slice));
            } else {
                let ptr = CPU_LOCAL[VcpuId()].AllocatorMut().alloc(layout);
                let slice = core::slice::from_raw_parts_mut(ptr, layout.size());
                return Ok(NonNull::new_unchecked(slice));
            }
        }
    }

    unsafe fn deallocate(&self, ptr: NonNull<u8>, layout: Layout) {
        let ptr = ptr.as_ptr();
        if !HostAllocator::IsHeapAddr( ptr as u64) {
            return GLOBAL_ALLOCATOR.dealloc(ptr, layout);
        }

        if !GUEST_HOST_SHARED_ALLOCATOR_INIT.load(Ordering::Relaxed) {
            return GLOBAL_ALLOCATOR.dealloc(ptr, layout);
        }
        if is_cc_active(){
            return PRIVATE_VCPU_SHARED_ALLOCATOR.AllocatorMut().dealloc(ptr, layout);
        } else {
            return CPU_LOCAL[VcpuId()].AllocatorMut().dealloc(ptr, layout);
        }
    }
}

impl UringAsyncMgr {
    pub fn FreeSlot(&self, id: usize) {
        self.freeSlot(id);
    }

    pub fn Clear(&self) {
        loop {
            let id = match self.freeids.lock().pop_front() {
                None => break,
                Some(id) => id,
            };
            self.freeSlot(id as _);
        }
    }
}

pub fn IsKernel() -> bool {
    return true;
}

pub fn ReapSwapIn() {
    HostSpace::SwapIn();
}

impl TsotSocketMgr {
    pub fn SendMsg(m: &TsotMessage) -> Result<()> {
        let res = HostSpace::TsotSendMsg(m as * const _ as u64);
        if res == 0 {
            return Ok(())
        }

        return Err(Error::SysError(SysErr::EINVAL));
    }

    pub fn RecvMsg() -> Result<TsotMessage> {
        let mut m = Box::new_in(TsotMessage::default(), GUEST_HOST_SHARED_ALLOCATOR);
        let res = HostSpace::TsotRecvMsg(&mut *m as * mut _ as u64);
        if res == 0 {
            return Ok(*m)
        }

        return Err(Error::SysError(SysErr::EINVAL));
    }
}

impl DnsSvc {
    pub fn Init(&self) -> Result<()> {
        panic!("impossible");
    }
}

/// enable access to EL0 memory from EL1 return the previous state
/// true : enabled, false : disabled
#[cfg(target_arch = "aarch64")]
#[inline]
pub fn enable_access_user() -> bool {
    #[cfg(target_feature = "pan")]
    {
        unsafe {
            // PAN==1 means access NOT allowed
            let allow = !pan();
            pan_set(false);
            return allow;
        }
    }
    // if PAN is not the case, accessing user memory is always enabled for EL1
    return true;
}

/// reset access to EL0 memory from EL1 return the previous state
/// true : enable false : disable
#[cfg(target_arch = "aarch64")]
#[inline]
pub fn set_access_user(_allow: bool) {
    #[cfg(target_feature = "pan")]
    {
        unsafe {
            pan_set(!_allow);
        }
    }
}

/// read raw opcode from userspace memory with pc;
/// unsafe: MUST make sure the mem page is present.
/// e.g. this exact user instruction has just triggered an exception
#[cfg(target_arch = "aarch64")]
pub unsafe fn read_user_opcode(pc: u64) -> Option<u32> {
    // this read is failiable
    // check pc alignment:
    if pc & 0b11 != 0 {
        return None;
    }
    let opcode: u32;
    let ua = enable_access_user();
    asm!("ldr {0:w}, [{1}]", out(reg) opcode, in(reg) pc);

    set_access_user(ua);
    return Some(opcode);
}
