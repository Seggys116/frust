fn leaf(n: usize) -> usize {
    n.wrapping_add(11)
}

pub fn generic<T: AsRef<[u8]>>(value: T) -> usize {
    value.as_ref().len().wrapping_add(0)
}

pub trait Measure {
    fn measure(&self) -> usize;
}

impl Measure for &str {
    fn measure(&self) -> usize {
        self.len().wrapping_add(0)
    }
}

pub fn api() -> usize {
    22
}

macro_rules! generated {
    () => {
        44usize
    };
}

fn main() {
    let native: usize = env!("FRUST_NATIVE_BYTES").parse().unwrap();
    println!(
        "{} {} {} {} {} {}",
        leaf(10),
        generic("ab"),
        "abcd".measure(),
        api(),
        generated!(),
        native
    );
}
