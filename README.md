# TinyWake

A minimal, `no_std`-compatible waker implementation for Cortex-M async executors.

## Features

- **No-std support**: Designed for bare-metal systems without the Rust standard library.
- **Lightweight**: Minimal overhead using `heapless` and `portable-atomic`.
- **Cortex-M Optimised**: Designed to work efficiently with the `cortex-m` architecture.

## Usage

Add this to your `Cargo.toml`:

```toml
[dependencies]
tinywake = "0.1.0"
```

### Example

```rust
use tinywake::run_all;

async fn my_task() {
    // Your async code here
}

fn main() {
    let mut task1 = my_task();
    run_all([&mut task1]);
}
```

## License

This project is licensed under the [MIT License](LICENSE).
