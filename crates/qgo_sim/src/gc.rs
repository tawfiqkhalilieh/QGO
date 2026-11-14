//! `qgo_sim::gc`: Tri-Color Mark-and-Sweep Tracing Garbage Collector for Classical QGo Heap Objects.
//!
//! Manages heap-allocated classical composite values (`[]T` slices, `struct` instances, and dynamic `string`s)
//! while Quantum Linear Registers (`qubit`, `qreg[N]`, `qint[N]`) are managed separately by
//! deterministic compile-time `defer adj` uncomputation and the Physical Qubit Pool (`StatevectorSim::free_qubit_pool`).

use std::collections::HashMap;

/// Handle into the managed classical heap arena.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct GcHandle(pub usize);

/// Tri-color marking state for Dijkstra/Steele-style tracing garbage collection.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GcColor {
    /// Unvisited candidate for reclamation during the sweep phase.
    White,
    /// Reachable from roots, pending traversal of its child references.
    Gray,
    /// Reachable from roots and all outgoing child references have been scanned.
    Black,
}

/// Heap-allocated classical object payload and outgoing reference edges.
#[derive(Debug, Clone)]
pub enum HeapObjKind {
    Str(String),
    Slice {
        len: usize,
        children: Vec<Option<GcHandle>>,
    },
    Struct {
        type_name: String,
        fields: HashMap<String, Option<GcHandle>>,
    },
}

/// A single slot in the managed classical heap arena.
#[derive(Debug, Clone)]
pub struct HeapSlot {
    pub id: usize,
    pub color: GcColor,
    pub size_bytes: usize,
    pub generation: u32,
    pub kind: HeapObjKind,
}

/// Tri-Color Mark-and-Sweep Garbage Collector for the QGo Classical Runtime.
#[derive(Debug, Clone)]
pub struct ClassicalHeapGC {
    pub arena: Vec<Option<HeapSlot>>,
    /// Free-list of swept arena indices available for $O(1)$ slot reuse.
    pub free_slots: Vec<usize>,
    pub live_objects: usize,
    pub live_bytes: usize,
    pub total_allocations: usize,
    pub total_swept_objects: usize,
    pub total_swept_bytes: usize,
    pub collections_run: usize,
    pub next_gc_threshold: usize,
}

impl Default for ClassicalHeapGC {
    fn default() -> Self {
        Self::new()
    }
}

impl ClassicalHeapGC {
    pub fn new() -> Self {
        Self {
            arena: Vec::new(),
            free_slots: Vec::new(),
            live_objects: 0,
            live_bytes: 0,
            total_allocations: 0,
            total_swept_objects: 0,
            total_swept_bytes: 0,
            collections_run: 0,
            next_gc_threshold: 64,
        }
    }

    fn insert_slot(&mut self, size_bytes: usize, kind: HeapObjKind) -> GcHandle {
        let id = if let Some(reused_idx) = self.free_slots.pop() {
            let slot = HeapSlot {
                id: reused_idx,
                color: GcColor::White,
                size_bytes,
                generation: 0,
                kind,
            };
            self.arena[reused_idx] = Some(slot);
            reused_idx
        } else {
            let idx = self.arena.len();
            let slot = HeapSlot {
                id: idx,
                color: GcColor::White,
                size_bytes,
                generation: 0,
                kind,
            };
            self.arena.push(Some(slot));
            idx
        };

        self.live_objects += 1;
        self.live_bytes += size_bytes;
        self.total_allocations += 1;
        GcHandle(id)
    }

    /// Allocates a managed heap string object.
    pub fn alloc_str(&mut self, s: &str) -> GcHandle {
        let bytes = 24 + s.len();
        self.insert_slot(bytes, HeapObjKind::Str(s.to_owned()))
    }

    /// Allocates a managed heap slice object with optional child handles for each element.
    pub fn alloc_slice(&mut self, children: Vec<Option<GcHandle>>, elem_bytes: usize) -> GcHandle {
        let len = children.len();
        let bytes = 24 + len * elem_bytes.max(8);
        self.insert_slot(bytes, HeapObjKind::Slice { len, children })
    }

    /// Allocates a managed heap struct instance with optional child handles for each field.
    pub fn alloc_struct(
        &mut self,
        type_name: &str,
        fields: HashMap<String, Option<GcHandle>>,
        base_bytes: usize,
    ) -> GcHandle {
        let bytes = 32 + fields.len() * 16 + base_bytes;
        self.insert_slot(
            bytes,
            HeapObjKind::Struct {
                type_name: type_name.to_owned(),
                fields,
            },
        )
    }

    /// Updates a field edge inside an existing heap struct object (write barrier).
    pub fn set_struct_field(
        &mut self,
        handle: GcHandle,
        field: &str,
        child: Option<GcHandle>,
    ) {
        if let Some(Some(slot)) = self.arena.get_mut(handle.0) {
            if let HeapObjKind::Struct { fields, .. } = &mut slot.kind {
                fields.insert(field.to_owned(), child);
            }
        }
    }

    /// Updates an element edge inside an existing heap slice object (write barrier).
    pub fn set_slice_elem(&mut self, handle: GcHandle, index: usize, child: Option<GcHandle>) {
        if let Some(Some(slot)) = self.arena.get_mut(handle.0) {
            if let HeapObjKind::Slice { children, len } = &mut slot.kind {
                if index >= children.len() {
                    children.resize(index + 1, None);
                    *len = children.len();
                }
                children[index] = child;
            }
        }
    }

    /// Returns `true` when heap growth has reached the adaptive collection threshold (`GOGC`).
    pub fn should_collect(&self) -> bool {
        self.live_objects >= self.next_gc_threshold
    }

    /// Executes a complete **Tri-Color Mark-and-Sweep** garbage collection cycle starting from `roots`.
    /// Returns the number of unreachable objects swept during this cycle.
    pub fn collect_garbage(&mut self, roots: &[GcHandle]) -> usize {
        // Phase 1: Reset all live arena slots to White
        for slot_opt in &mut self.arena {
            if let Some(slot) = slot_opt {
                slot.color = GcColor::White;
            }
        }

        // Shade root objects Gray and push onto the marking worklist
        let mut gray_worklist: Vec<usize> = Vec::with_capacity(roots.len());
        for &GcHandle(root_id) in roots {
            if let Some(Some(slot)) = self.arena.get_mut(root_id) {
                if slot.color == GcColor::White {
                    slot.color = GcColor::Gray;
                    gray_worklist.push(root_id);
                }
            }
        }

        // Phase 2: Transitive Tri-Color Tracing (Gray -> Black)
        while let Some(curr_id) = gray_worklist.pop() {
            let mut child_handles: Vec<GcHandle> = Vec::new();
            if let Some(Some(slot)) = self.arena.get_mut(curr_id) {
                slot.color = GcColor::Black;
                slot.generation = slot.generation.saturating_add(1);
                match &slot.kind {
                    HeapObjKind::Str(_) => {}
                    HeapObjKind::Slice { children, .. } => {
                        for c in children.iter().flatten() {
                            child_handles.push(*c);
                        }
                    }
                    HeapObjKind::Struct { fields, .. } => {
                        for c in fields.values().flatten() {
                            child_handles.push(*c);
                        }
                    }
                }
            }

            for GcHandle(child_id) in child_handles {
                if let Some(Some(child_slot)) = self.arena.get_mut(child_id) {
                    if child_slot.color == GcColor::White {
                        child_slot.color = GcColor::Gray;
                        gray_worklist.push(child_id);
                    }
                }
            }
        }

        // Phase 3: Sweep White (unreachable) objects and recycle their arena slots
        let mut swept_count = 0usize;
        let mut swept_bytes = 0usize;
        for idx in 0..self.arena.len() {
            let is_unreachable = matches!(
                self.arena[idx],
                Some(HeapSlot {
                    color: GcColor::White,
                    ..
                })
            );
            if is_unreachable {
                if let Some(dead) = self.arena[idx].take() {
                    swept_count += 1;
                    swept_bytes += dead.size_bytes;
                    self.live_objects = self.live_objects.saturating_sub(1);
                    self.live_bytes = self.live_bytes.saturating_sub(dead.size_bytes);
                    self.free_slots.push(idx);
                }
            }
        }

        self.total_swept_objects += swept_count;
        self.total_swept_bytes += swept_bytes;
        self.collections_run += 1;
        // Go-style GOGC=100 adaptive heap pacing
        self.next_gc_threshold = (self.live_objects * 2).max(64);

        swept_count
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_tricolor_mark_and_sweep_nested_and_cyclic_graphs() {
        let mut gc = ClassicalHeapGC::new();

        // 1. Allocate a reachable subgraph: RootStruct -> Slice -> LeafStr
        let leaf_str = gc.alloc_str("quantum_payload");
        let mid_slice = gc.alloc_slice(vec![Some(leaf_str), None], 16);
        let mut root_fields = HashMap::new();
        root_fields.insert("items".to_owned(), Some(mid_slice));
        let root_struct = gc.alloc_struct("Packet", root_fields, 16);

        // 2. Allocate an unreachable cyclic subgraph: NodeA <-> NodeB + DeadStr
        let dead_str = gc.alloc_str("ephemeral_garbage");
        let mut a_fields = HashMap::new();
        a_fields.insert("label".to_owned(), Some(dead_str));
        let node_a = gc.alloc_struct("Node", a_fields, 16);
        let mut b_fields = HashMap::new();
        b_fields.insert("peer".to_owned(), Some(node_a));
        let node_b = gc.alloc_struct("Node", b_fields, 16);
        // Close the cycle: NodeA.peer -> NodeB
        gc.set_struct_field(node_a, "peer", Some(node_b));

        assert_eq!(gc.live_objects, 6);

        // 3. Run Tri-Color Mark-and-Sweep with only `root_struct` as a root
        let swept = gc.collect_garbage(&[root_struct]);
        assert_eq!(swept, 3, "dead_str, node_a, and node_b must be swept");
        assert_eq!(gc.live_objects, 3, "root_struct, mid_slice, and leaf_str must survive");
        assert_eq!(gc.total_swept_objects, 3);
        assert_eq!(gc.free_slots.len(), 3);

        // 4. Allocate a new object and verify it recycles one of the swept arena slots
        let recycled_h = gc.alloc_str("reused_slot");
        assert!(recycled_h.0 < 6, "should reuse an existing arena slot from free_slots");
        assert_eq!(gc.free_slots.len(), 2);
    }
}
