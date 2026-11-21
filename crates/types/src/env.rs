//! Run-time knobs, each read once at start, never per row.

/// A numeric knob, `default` when unset.
pub fn number(var: &str, default: usize) -> usize {
    match std::env::var(var) {
        Ok(value) => value.parse().unwrap_or_else(|_| panic!("{var}={value}: not a number")),
        Err(_) => default,
    }
}

/// Which implementation a slot runs: the position of the env var's value among the
/// compiled-in `options`, or 0 (the pick, listed first) when the var is unset.
pub fn pick(var: &str, options: &[&str]) -> usize {
    match std::env::var(var) {
        Ok(value) => options
            .iter()
            .position(|option| *option == value)
            .unwrap_or_else(|| panic!("{var}={value}: compiled in are {options:?}")),
        Err(_) => 0,
    }
}
