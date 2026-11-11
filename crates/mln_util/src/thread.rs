//! The threads mln-rs starts.

/// The stack size of every thread mln-rs starts. Parsing and evaluating
/// style JSON, filters and expressions recurses once per nesting level, so
/// a thread gets this much whatever `RUST_MIN_STACK` or the platform's
/// default would give it.
pub const STACK_SIZE: usize = 2 * 1024 * 1024;

/// A builder for a thread named `name` with a [`STACK_SIZE`] stack.
pub fn builder(name: impl Into<String>) -> std::thread::Builder {
    std::thread::Builder::new().name(name.into()).stack_size(STACK_SIZE)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_the_thread() {
        let name = builder("parse").spawn(|| std::thread::current().name().map(str::to_owned)).unwrap().join();
        assert_eq!(name.unwrap().as_deref(), Some("parse"));
    }
}
