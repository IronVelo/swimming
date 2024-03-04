# swimming

`swimming` is a `no_std`, `no_alloc`, high-performance, asynchronous, thread-safe, and fair connection pooling crate for
Rust. It provides a simple and efficient way to manage and reuse connections in your applications.

## Features

- `no_std` and `no_alloc` compatibility
- High performance with lazy and eager connection initialization
- Asynchronous and thread-safe operations
- Fair connection retrieval mechanism
- Flexible and extensible design
- Robust error handling
- Environment agnostic

## Installation

To use `swimming` in your Rust project, add the following to your `Cargo.toml` file:

```toml
[dependencies]
swimming = "0.1.1"
```

## Usage

Here's a simple example of how to use `swimming`:
```rust
use swimming::{Pool, Connection};
use std::net::SocketAddr;

struct MyConnection;

impl Connection for MyConnection {
    type Context = SocketAddr;
    type Error = ();

    async fn create(_ctx: &SocketAddr) -> Result<Self, Self::Error> {
        Ok(MyConnection)
    }

    fn needs_health_check(&mut self, _ctx: &SocketAddr) -> bool {
        false
    }

    async fn health_check(&mut self, _ctx: &SocketAddr) -> Result<(), ()> {
        Ok(())
    }
}

async fn example() {
    // create the pool providing the context, in general usage this would be details primarily pertinent to creating
    // the connection. 
    let pool = Pool::<MyConnection, 10>::new("127.0.0.1:8080".parse().unwrap());

    let conn = pool.get().await.unwrap();
    // Use the connection...
}
```

For more detailed usage examples and API documentation, please refer to the `Pool`'s documentation.

## License

This crate is licensed under the MIT or Apache 2.0 license at your preference.

