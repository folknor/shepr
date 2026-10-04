struct Fnv64(u64);

impl Fnv64 {
    fn new() -> Self {
        Self(0xcbf2_9ce4_8422_2325)
    }

    fn write_byte(&mut self, byte: u8) {
        self.0 ^= u64::from(byte);
        self.0 = self.0.wrapping_mul(0x0000_0100_0000_01b3);
    }

    fn write(&mut self, bytes: &[u8]) {
        for byte in bytes {
            self.write_byte(*byte);
        }
    }

    fn finish(self) -> u64 {
        self.0
    }
}

pub(in crate::shell) fn fnv1a64(bytes: &[u8]) -> u64 {
    let mut hash = Fnv64::new();
    hash.write(bytes);
    hash.finish()
}
