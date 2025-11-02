pub trait Sequence: Sized {
    fn get_index_from(self, mask: usize) -> usize;
}

impl Sequence for i64 {
    fn get_index_from(self, mask: usize) -> usize {
        (self as usize) & mask
    }
}
