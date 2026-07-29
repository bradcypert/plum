use crate::Value;

// A simulated heap, standing in for real memory so refcounting and
// reuse-in-place have something concrete to act on. Deliberately does
// its OWN refcounting rather than piggybacking on Rust's `Rc<T>` —
// using Rc would silently make everything "just work" regardless of
// whether OUR analysis (fbip.rs) placed Inc/Dec/CtorReuse correctly,
// which defeats the point: this heap exists to catch bugs in that
// analysis, not paper over them.
//
// To that end, misuse is DETECTED, not undefined: reading, incrementing,
// or decrementing a freed cell is a reported error, not silent
// corruption. If FBIP's analysis is ever wrong, this heap should be
// where that shows up, as a test failure here — not as mysteriously
// wrong output somewhere else downstream.
//
// A cell holds one of two shapes — a tagged constructor (structs,
// tuples, enum variants, arrays: all reuse the same `Ctor` shape, see
// `lower.rs`'s synthetic-tag convention) or a raw string buffer. These
// are DELIBERATELY kept as separate `CellData` variants rather than
// representing a string as a `Ctor` whose fields are one-per-character
// (which the existing `Vec<Value>` machinery could technically hold):
// there's no `Value::Char` in this language, and a `Vec<Value::Int>`
// per codepoint would be wildly wasteful for anything but toy strings.
enum CellData {
    Ctor { tag: String, fields: Vec<Value> },
    Str(String),
}

struct HeapCell {
    data: CellData,
    refcount: usize,
}

/// A borrowed, shape-tagged view of a cell's contents — see
/// `Heap::read_any`.
pub enum CellView<'a> {
    Ctor(&'a str, &'a [Value]),
    Str(&'a str),
}

pub struct Heap {
    cells: Vec<Option<HeapCell>>,
    // Total number of times `alloc`/`alloc_str` actually allocated a NEW
    // cell — does not count `dec_and_maybe_reuse`/`dec_and_maybe_reuse_
    // str` overwriting an existing one. This is what proves reuse-in-
    // place saved a real allocation, not just that a `CtorReuse`/`*Reuse`
    // node was present.
    alloc_count: usize,
}

impl Heap {
    pub fn new() -> Self {
        Heap {
            cells: Vec::new(),
            alloc_count: 0,
        }
    }

    pub fn alloc_count(&self) -> usize {
        self.alloc_count
    }

    pub fn alloc(&mut self, tag: String, fields: Vec<Value>) -> usize {
        self.cells.push(Some(HeapCell {
            data: CellData::Ctor { tag, fields },
            refcount: 1,
        }));
        self.alloc_count += 1;
        self.cells.len() - 1
    }

    /// Allocates a fresh string cell — see this module's doc comment
    /// for why strings get their own `CellData` shape instead of
    /// piggybacking on `Ctor`.
    pub fn alloc_str(&mut self, s: String) -> usize {
        self.cells.push(Some(HeapCell {
            data: CellData::Str(s),
            refcount: 1,
        }));
        self.alloc_count += 1;
        self.cells.len() - 1
    }

    fn cell(&self, addr: usize) -> Result<&HeapCell, String> {
        match self.cells.get(addr) {
            Some(Some(cell)) => Ok(cell),
            Some(None) => Err(format!("use after free at heap address {addr}")),
            None => Err(format!("invalid heap address {addr}")),
        }
    }

    fn cell_mut(&mut self, addr: usize) -> Result<&mut HeapCell, String> {
        match self.cells.get_mut(addr) {
            Some(Some(cell)) => Ok(cell),
            Some(None) => Err(format!("use after free at heap address {addr}")),
            None => Err(format!("invalid heap address {addr}")),
        }
    }

    /// Reads a `Ctor`-shaped cell's tag and fields. Errors (rather than
    /// panicking) if `addr` names a string cell instead — every caller
    /// of this method (`Match`, `Index`, `.push()`/etc.) only ever
    /// makes sense for a tagged constructor, so a string here means the
    /// program's own logic (or a real bug upstream) confused the two.
    pub fn read(&self, addr: usize) -> Result<(&str, &[Value]), String> {
        match &self.cell(addr)?.data {
            CellData::Ctor { tag, fields } => Ok((tag, fields)),
            CellData::Str(_) => Err(format!("expected a constructor value, found a string at heap address {addr}")),
        }
    }

    /// Reads a string cell's contents. Errors if `addr` names a `Ctor`
    /// cell instead — the exact counterpart to `read`'s own check.
    pub fn read_str(&self, addr: usize) -> Result<&str, String> {
        match &self.cell(addr)?.data {
            CellData::Str(s) => Ok(s),
            CellData::Ctor { .. } => Err(format!("expected a string value, found a constructor at heap address {addr}")),
        }
    }

    /// Reads a cell without assuming its shape ahead of time — for the
    /// handful of callers (`to_portable`, `ArrayLen`'s eval case,
    /// heap-value equality) that need to dispatch on whichever shape a
    /// `Value::HeapRef` actually points to, rather than asserting one.
    pub fn read_any(&self, addr: usize) -> Result<CellView<'_>, String> {
        match &self.cell(addr)?.data {
            CellData::Ctor { tag, fields } => Ok(CellView::Ctor(tag, fields)),
            CellData::Str(s) => Ok(CellView::Str(s)),
        }
    }

    pub fn refcount(&self, addr: usize) -> Result<usize, String> {
        Ok(self.cell(addr)?.refcount)
    }

    pub fn inc(&mut self, addr: usize) -> Result<(), String> {
        self.cell_mut(addr)?.refcount += 1;
        Ok(())
    }

    pub fn dec(&mut self, addr: usize) -> Result<(), String> {
        let cell = self.cell_mut(addr)?;
        cell.refcount -= 1;
        if cell.refcount == 0 {
            self.cells[addr] = None;
        }
        Ok(())
    }

    /// Decrements `addr`'s refcount; if that reaches zero, `addr` was
    /// the last owner, so its memory is recycled in place for the new
    /// (tag, fields) and the SAME address is returned. Otherwise the
    /// cell is still shared — it's left untouched (just decremented)
    /// and a genuinely new cell is allocated instead.
    pub fn dec_and_maybe_reuse(&mut self, addr: usize, tag: String, fields: Vec<Value>) -> Result<usize, String> {
        let cell = self.cell_mut(addr)?;
        cell.refcount -= 1;
        if cell.refcount == 0 {
            self.cells[addr] = Some(HeapCell {
                data: CellData::Ctor { tag, fields },
                refcount: 1,
            });
            Ok(addr)
        } else {
            Ok(self.alloc(tag, fields))
        }
    }

    /// The string counterpart to `dec_and_maybe_reuse` — same exact
    /// refcount-gated recycle-or-allocate-fresh decision, just for a
    /// string cell's new contents instead of a `Ctor`'s tag/fields.
    pub fn dec_and_maybe_reuse_str(&mut self, addr: usize, new_str: String) -> Result<usize, String> {
        let cell = self.cell_mut(addr)?;
        cell.refcount -= 1;
        if cell.refcount == 0 {
            self.cells[addr] = Some(HeapCell {
                data: CellData::Str(new_str),
                refcount: 1,
            });
            Ok(addr)
        } else {
            Ok(self.alloc_str(new_str))
        }
    }
}

impl Default for Heap {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn alloc_then_read() {
        let mut heap = Heap::new();
        let addr = heap.alloc("Point".to_string(), vec![Value::Int(1), Value::Int(2)]);
        let (tag, fields) = heap.read(addr).unwrap();
        assert_eq!(tag, "Point");
        assert_eq!(fields, &[Value::Int(1), Value::Int(2)]);
    }

    #[test]
    fn alloc_str_then_read_str() {
        let mut heap = Heap::new();
        let addr = heap.alloc_str("hello".to_string());
        assert_eq!(heap.read_str(addr).unwrap(), "hello");
    }

    #[test]
    fn reading_a_string_cell_as_a_ctor_is_a_detected_error() {
        let mut heap = Heap::new();
        let addr = heap.alloc_str("hello".to_string());
        let err = heap.read(addr).expect_err("expected reading a string cell as a Ctor to fail");
        assert!(err.contains("string"), "error should mention the mismatch, got: {err}");
    }

    #[test]
    fn reading_a_ctor_cell_as_a_string_is_a_detected_error() {
        let mut heap = Heap::new();
        let addr = heap.alloc("Point".to_string(), vec![]);
        let err = heap.read_str(addr).expect_err("expected reading a Ctor cell as a string to fail");
        assert!(err.contains("constructor"), "error should mention the mismatch, got: {err}");
    }

    #[test]
    fn fresh_alloc_starts_at_refcount_one() {
        let mut heap = Heap::new();
        let addr = heap.alloc("Point".to_string(), vec![]);
        assert_eq!(heap.refcount(addr).unwrap(), 1);
    }

    #[test]
    fn fresh_alloc_str_starts_at_refcount_one() {
        let mut heap = Heap::new();
        let addr = heap.alloc_str("hi".to_string());
        assert_eq!(heap.refcount(addr).unwrap(), 1);
    }

    #[test]
    fn inc_increases_refcount() {
        let mut heap = Heap::new();
        let addr = heap.alloc("Point".to_string(), vec![]);
        heap.inc(addr).unwrap();
        assert_eq!(heap.refcount(addr).unwrap(), 2);
    }

    #[test]
    fn dec_decreases_refcount_without_freeing_while_above_zero() {
        let mut heap = Heap::new();
        let addr = heap.alloc("Point".to_string(), vec![]);
        heap.inc(addr).unwrap();
        heap.dec(addr).unwrap();
        assert_eq!(heap.refcount(addr).unwrap(), 1);
        // Still readable — wasn't freed.
        assert!(heap.read(addr).is_ok());
    }

    #[test]
    fn dec_to_zero_frees_the_cell() {
        let mut heap = Heap::new();
        let addr = heap.alloc("Point".to_string(), vec![]);
        heap.dec(addr).unwrap();
        let err = heap.read(addr).expect_err("expected reading a freed cell to fail");
        assert!(err.contains("free"), "error should mention freeing, got: {err}");
    }

    #[test]
    fn double_free_is_a_detected_error_not_corruption() {
        let mut heap = Heap::new();
        let addr = heap.alloc("Point".to_string(), vec![]);
        heap.dec(addr).unwrap(); // refcount 1 -> 0, freed
        let err = heap.dec(addr).expect_err("expected decrementing an already-freed cell to fail");
        assert!(err.contains("free"), "error should mention double free, got: {err}");
    }

    #[test]
    fn inc_on_freed_cell_is_a_detected_error() {
        let mut heap = Heap::new();
        let addr = heap.alloc("Point".to_string(), vec![]);
        heap.dec(addr).unwrap();
        assert!(heap.inc(addr).is_err());
    }

    #[test]
    fn invalid_address_is_a_detected_error() {
        let heap = Heap::new();
        assert!(heap.read(999).is_err());
    }

    #[test]
    fn reuse_when_uniquely_owned_recycles_the_same_address() {
        let mut heap = Heap::new();
        let addr = heap.alloc("Point".to_string(), vec![Value::Int(1), Value::Int(2)]);
        let allocs_before = heap.alloc_count();
        let result_addr = heap
            .dec_and_maybe_reuse(addr, "Point".to_string(), vec![Value::Int(2), Value::Int(1)])
            .unwrap();
        assert_eq!(result_addr, addr, "should reuse the exact same address");
        assert_eq!(heap.alloc_count(), allocs_before, "reuse must not count as a fresh allocation");
        let (tag, fields) = heap.read(result_addr).unwrap();
        assert_eq!(tag, "Point");
        assert_eq!(fields, &[Value::Int(2), Value::Int(1)]);
    }

    #[test]
    fn reuse_when_shared_allocates_fresh_and_preserves_the_original() {
        let mut heap = Heap::new();
        let addr = heap.alloc("Point".to_string(), vec![Value::Int(1), Value::Int(2)]);
        heap.inc(addr).unwrap(); // now shared: refcount 2
        let allocs_before = heap.alloc_count();

        let result_addr = heap
            .dec_and_maybe_reuse(addr, "Point".to_string(), vec![Value::Int(9), Value::Int(9)])
            .unwrap();

        assert_ne!(result_addr, addr, "must not reuse memory that's still shared");
        assert_eq!(heap.alloc_count(), allocs_before + 1, "the shared path must allocate fresh");

        // The original cell survives, untouched, at its original
        // refcount (2 - 1 = 1 after this operation released one share).
        let (orig_tag, orig_fields) = heap.read(addr).unwrap();
        assert_eq!(orig_tag, "Point");
        assert_eq!(orig_fields, &[Value::Int(1), Value::Int(2)]);
        assert_eq!(heap.refcount(addr).unwrap(), 1);
    }

    #[test]
    fn reuse_str_when_uniquely_owned_recycles_the_same_address() {
        let mut heap = Heap::new();
        let addr = heap.alloc_str("hello".to_string());
        let allocs_before = heap.alloc_count();
        let result_addr = heap.dec_and_maybe_reuse_str(addr, "hello world".to_string()).unwrap();
        assert_eq!(result_addr, addr, "should reuse the exact same address");
        assert_eq!(heap.alloc_count(), allocs_before, "reuse must not count as a fresh allocation");
        assert_eq!(heap.read_str(result_addr).unwrap(), "hello world");
    }

    #[test]
    fn reuse_str_when_shared_allocates_fresh_and_preserves_the_original() {
        let mut heap = Heap::new();
        let addr = heap.alloc_str("hello".to_string());
        heap.inc(addr).unwrap(); // now shared: refcount 2
        let allocs_before = heap.alloc_count();

        let result_addr = heap.dec_and_maybe_reuse_str(addr, "hello world".to_string()).unwrap();

        assert_ne!(result_addr, addr, "must not reuse memory that's still shared");
        assert_eq!(heap.alloc_count(), allocs_before + 1, "the shared path must allocate fresh");
        assert_eq!(heap.read_str(addr).unwrap(), "hello");
        assert_eq!(heap.refcount(addr).unwrap(), 1);
        assert_eq!(heap.read_str(result_addr).unwrap(), "hello world");
    }
}
