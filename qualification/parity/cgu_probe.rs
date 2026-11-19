macro_rules! probe_mod {
    ($name:ident, $seed:expr) => {
        mod $name {
            #[inline(never)]
            pub fn item(n: u32) -> u32 {
                n.wrapping_mul($seed).wrapping_add($seed)
            }
        }
    };
}

probe_mod!(m0, 3);
probe_mod!(m1, 5);
probe_mod!(m2, 7);
probe_mod!(m3, 9);
probe_mod!(m4, 11);
probe_mod!(m5, 13);
probe_mod!(m6, 17);
probe_mod!(m7, 19);

fn main() {
    let total = m0::item(1)
        + m1::item(1)
        + m2::item(1)
        + m3::item(1)
        + m4::item(1)
        + m5::item(1)
        + m6::item(1)
        + m7::item(1);
    println!("{total}");
}
