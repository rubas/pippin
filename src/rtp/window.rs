//! The packets seen near the newest one, as in the replay list of
//! RFC 3711 3.3.2.

/// The highest index seen, and a bit for each of the `64 * WORDS` indices
/// up to it. The bits are a ring: index `i` has bit `i % SIZE`.
pub struct SeenWindow<const WORDS: usize> {
    top: u64,
    seen: [u64; WORDS],
}

impl<const WORDS: usize> SeenWindow<WORDS> {
    /// How far behind the top an index can still be told apart.
    pub const SIZE: u64 = 64 * WORDS as u64;

    /// Starts at the first index seen.
    pub fn new(first: u64) -> Self {
        let mut w = Self { top: first, seen: [0; WORDS] };
        w.set(first);
        w
    }

    /// The highest index seen.
    pub fn top(&self) -> u64 {
        self.top
    }

    fn bit(index: u64) -> (usize, u64) {
        let i = index % Self::SIZE;
        ((i / 64) as usize, 1 << (i % 64))
    }

    fn set(&mut self, index: u64) {
        let (w, b) = Self::bit(index);
        self.seen[w] |= b;
    }

    /// Marks `index` seen. False if it was seen before, or is `SIZE` or more
    /// behind the top, where the window cannot tell.
    pub fn insert(&mut self, index: u64) -> bool {
        if index > self.top {
            // Forget the slots that the new indices reuse.
            if index - self.top >= Self::SIZE {
                self.seen = [0; WORDS];
            } else {
                for i in self.top + 1..index {
                    let (w, b) = Self::bit(i);
                    self.seen[w] &= !b;
                }
            }
            self.top = index;
            self.set(index);
            return true;
        }
        if self.top - index >= Self::SIZE {
            return false;
        }
        let (w, b) = Self::bit(index);
        let new = self.seen[w] & b == 0;
        self.seen[w] |= b;
        new
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn each_index_is_new_once() {
        let mut w = SeenWindow::<1>::new(1000);
        assert!(!w.insert(1000));
        assert!(w.insert(1002));
        assert!(w.insert(1001));
        assert!(!w.insert(1001));
        assert!(!w.insert(1002));
        assert_eq!(w.top(), 1002);
    }

    #[test]
    fn indices_too_far_behind_are_refused() {
        let mut w = SeenWindow::<1>::new(1000);
        assert!(w.insert(1100));
        // 63 behind is the last slot; 64 behind is outside.
        assert!(w.insert(1037));
        assert!(!w.insert(1036));
        assert!(!w.insert(999));
    }

    #[test]
    fn a_slot_reused_after_a_jump_is_new_again() {
        let mut w = SeenWindow::<1>::new(1000);
        assert!(w.insert(1001));
        // 1065 reuses the slot of 1001 and the slots of 1002 to 1064 are
        // cleared on the way, so 1010 counts as new.
        assert!(w.insert(1065));
        assert!(w.insert(1010));
        // A jump past the whole window clears every slot.
        assert!(w.insert(5000));
        assert!(w.insert(4990));
        assert!(!w.insert(4990));
    }
}
