use super::kernel32;
use x86::Mem;

pub trait Alloc {
    fn alloc(&mut self, size: u32) -> u32;
    fn size(&self, addr: u32) -> u32;
    fn free(&mut self, addr: u32);
}

#[derive(serde::Serialize, serde::Deserialize)]
pub struct ArenaInfo {
    pub addr: u32,
    pub size: u32,
    next: u32,
}
impl ArenaInfo {
    pub fn new(addr: u32, size: u32) -> Self {
        ArenaInfo {
            addr,
            size,
            next: 0,
        }
    }
    pub fn get<'a, 'm>(&'a mut self, mem: &'a mut Mem<'m>) -> Arena<'a, 'm> {
        Arena { info: self, mem }
    }
}

pub struct Arena<'a, 'm> {
    info: &'a mut ArenaInfo,
    mem: &'a mut Mem<'m>,
}

fn align32(n: u32) -> u32 {
    (n + 3) & !3
}

impl<'a, 'm> Alloc for Arena<'a, 'm> {
    fn alloc(&mut self, size: u32) -> u32 {
        let alloc_size = align32(size + 4);
        if self.info.next + alloc_size > self.info.size {
            log::error!(
                "Arena::alloc cannot allocate {:x}, using {:x}/{:x}",
                size,
                self.info.size - self.info.next,
                self.info.size
            );
            return 0;
        }
        let addr = self.info.addr + self.info.next;
        self.mem.put::<u32>(addr, size);
        self.info.next += alloc_size;
        addr + 4
    }

    fn size(&self, addr: u32) -> u32 {
        assert!(addr >= self.info.addr + 4 && addr < self.info.addr + self.info.size);
        self.mem.get::<u32>(addr - 4)
    }

    fn free(&mut self, _addr: u32) {
        unimplemented!("can't free from arena")
    }
}

#[derive(serde::Serialize, serde::Deserialize)]
pub struct HeapInfo {
    pub addr: u32,
    pub size: u32,
    /// Pointer to first free block: head of the FreeNode list.
    free: u32,
}

impl HeapInfo {
    pub fn new(mem: &mut Mem, addr: u32, size: u32) -> Self {
        *FreeNode::get(mem, addr) = FreeNode { size, next: 0 };
        HeapInfo {
            addr,
            size,
            free: addr,
        }
    }

    /// Attempt to coalesce the freelist node at addr with any subsequent
    /// adjacent blocks of free memory.
    fn try_coalesce(&mut self, mem: &mut Mem, addr: u32) {
        loop {
            let FreeNode { next, size } = *FreeNode::get(mem, addr);
            if next != addr + size {
                break;
            }
            let next = FreeNode::get(mem, next);
            *FreeNode::get(mem, addr) = FreeNode {
                next: next.next,
                size: size + next.size,
            }
        }
    }

    pub fn get_heap<'a>(
        &'a mut self,
        mem: &'a mut Mem<'a>,
        mappings: &'a mut kernel32::Mappings,
    ) -> Heap<'a> {
        Heap {
            info: self,
            mem,
            mappings,
        }
    }
}

pub struct Heap<'a> {
    info: &'a mut HeapInfo,
    mem: &'a mut Mem<'a>,
    mappings: &'a mut kernel32::Mappings,
}

#[derive(Debug)]
#[repr(C)]
struct FreeNode {
    size: u32,
    /// Pointer to next node.
    next: u32,
}
unsafe impl x86::Pod for FreeNode {}
impl FreeNode {
    fn get<'a>(mem: &'a mut Mem, addr: u32) -> &'a mut Self {
        mem.view_mut::<FreeNode>(addr)
    }
}

impl<'a> Alloc for Heap<'a> {
    fn alloc(&mut self, size: u32) -> u32 {
        let alloc_size = align32(size + 4);

        // Find a FreeNode large enough to accommodate alloc_size.
        // To use it, update the previous node to point past it.
        let mut prev = 0;
        let mut cur = self.info.free;
        let mut blocks = 0;
        while cur != 0 {
            blocks += 1;
            let node = FreeNode::get(self.mem, cur);
            if node.size >= alloc_size {
                break;
            }
            if node.next == 0 {
                // Reached last node, try resizing before giving up.
                let space_needed = alloc_size - node.size;
                node.size += self.mappings.grow(self.info.addr, space_needed);
                if node.size < alloc_size {
                    panic!("heap OOM allocating {alloc_size:#x}: resized, but still too small");
                }
                break;
            }
            prev = cur;
            cur = node.next;
        }
        if cur == 0 {
            panic!("heap OOM allocating {alloc_size:#x} freelist {blocks} entries");
        }

        // Find the pointer to the point after the allocated block.
        let next = if FreeNode::get(self.mem, cur).size > alloc_size + 8 {
            // Split cur block into smaller piece; create a new FreeNode in
            // the remaining space.
            let next = cur + alloc_size;
            let cur = FreeNode::get(self.mem, cur);
            *FreeNode::get(self.mem, next) = FreeNode {
                size: cur.size - alloc_size,
                next: cur.next,
            };
            next
        } else {
            FreeNode::get(self.mem, cur).next
        };

        // Link next node into the list.
        if prev == 0 {
            self.info.free = next;
        } else {
            FreeNode::get(self.mem, prev).next = next;
        }

        self.mem.put::<u32>(cur, size);
        cur + 4
    }

    fn size(&self, addr: u32) -> u32 {
        self.mem.get::<u32>(addr - 4)
    }

    fn free(&mut self, addr: u32) {
        let free_size = self.size(addr) + 4;
        let addr = addr - 4;

        let mut prev = 0;
        let mut next = self.info.free;
        while next < addr {
            prev = next;
            next = FreeNode::get(self.mem, next).next;
        }

        // Insert freelist node at addr.
        *FreeNode::get(self.mem, addr) = FreeNode {
            next,
            size: free_size,
        };
        if prev > 0 {
            FreeNode::get(self.mem, prev).next = addr;
            self.info.try_coalesce(self.mem, prev);
        } else {
            self.info.free = addr;
            self.info.try_coalesce(self.mem, addr);
        }
    }
}
