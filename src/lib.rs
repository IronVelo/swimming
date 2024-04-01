#![doc = include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/README.md"))]
#![cfg_attr(not(feature = "std"), no_std)]
#![no_builtins]

#![warn(
    clippy::all,
    clippy::nursery,
    clippy::pedantic,
    clippy::cargo,
)]
#![allow(
    clippy::cargo_common_metadata,
    clippy::module_name_repetitions,
    clippy::multiple_crate_versions,
)]

use lock_pool::{LockGuard, LockPool, maybe_await};
use core::future::Future;
use core::mem::MaybeUninit;
use core::ops::{Deref, DerefMut};

const MAX_WAITERS: usize = 32;

/// # Connection Management
///
/// The Connection trait defines an asynchronous interface for managing connections in a concurrent
/// environment. Implementors of this trait can represent any form of connection, such as to a
/// database, network service, or any other resource that requires managing a persistent connection
/// state.
///
/// # Example
///
/// ```
/// use swimming::Connection;
///
/// struct MyConn;
///
/// impl Connection for MyConn {
///     type Context = ();
///     type Error = ();
///
///     async fn create(_ctx: &()) -> Result<Self, ()> {
///         Ok(MyConn)
///     }
///
///     fn needs_health_check(&mut self, _ctx: &()) -> bool {
///         // always perform a health check.
///         // (in practice you would want to check if conn is stale)
///         true
///     }
///
///     async fn health_check(&mut self, _ctx: &()) -> Result<(), ()> {
///         // MyConn is always healthy. (In practice you'd do something like ping the server)
///         Ok(())
///     }
/// }
/// ```
pub trait Connection: Sized + Send + Sync {
    /// The state of the pool, passed to the `Connection` methods. Generally useful for storing
    /// details pertinent to the connection.
    type Context: Send + Sync;
    /// The error type that operations of this trait may return.
    type Error: Send + Sync;
    /// # Create Connection
    ///
    /// Creates a new instance of the connection asynchronously. This method should handle the
    /// necessary initialization for the connection, such as establishing a network connection,
    /// authenticating, or any other setup required before the connection can be used.
    ///
    /// # Returns
    ///
    /// - `Ok(Self)`:         The connection was successfully created and setup.
    /// - `Err(Self::Error)`: There was an error creating / setting up the connection.
    fn create(ctx: &Self::Context) -> impl Future<Output = Result<Self, Self::Error>> + Send + Sync;
    /// # Needs Health Check
    ///
    /// Performs a quick, lightweight check to determine whether a more comprehensive health check
    /// [`health_check`] needs to be run. This method is intended to be fast and should not perform
    /// any heavy computations or network operations.
    ///
    /// # Returns
    ///
    /// - `true`:  A full [`health_check`] is warranted.
    /// - `false`: The connection is fine to use without any health check.
    ///
    /// [`health_check`]: Connection::needs_health_check
    fn needs_health_check(&mut self, ctx: &Self::Context) -> bool;
    /// # Health Check
    ///
    /// Asynchronously checks the health of the connection, verifying that it is still viable for
    /// use. This method should be implemented to perform any necessary checks to ensure the
    /// connection's integrity and operational status, such as sending a ping to a server or
    /// validating a session token.
    ///
    /// <br>
    ///
    /// If this fails, the pool will attempt to create a new connection. If this is not possible the
    /// pool will return the [`Error`] from the [`create`] implementation to threads requesting
    /// connections.
    ///
    /// <br>
    ///
    /// Implementors should ensure that [`needs_health_check`] is as efficient as possible to avoid
    /// degrading performance. The `health_check` method may involve asynchronous operations and
    /// should be prepared to handle timeouts and other common network-related issues gracefully.
    ///
    /// # Returns
    ///
    /// - `Ok(())`:           The connection is OK to continue using.
    /// - `Err(Self::Error)`: The connection needs to be restarted.
    ///
    /// [`needs_health_check`]: Connection::needs_health_check
    /// [`create`]: Connection::create
    /// [`Error`]: Connection::Error
    fn health_check(&mut self, ctx: &Self::Context) -> impl Future<Output = Result<(), Self::Error>> + Send + Sync;
}

type Guard<'pool, Conn, const SIZE: usize> = LockGuard<'pool, LazyConn<Conn>, SIZE, MAX_WAITERS>;

#[doc(hidden)]
pub struct LazyConn<Conn> {
    conn: Option<Conn>
}

impl<Conn> Default for LazyConn<Conn> {
    #[inline]
    fn default() -> Self {
        Self::new()
    }
}

impl<Conn> LazyConn<Conn> {
    #[must_use] const fn new() -> Self {
        Self {
            conn: None
        }
    }
    
    const fn init(conn: Conn) -> Self {
        Self {
            conn: Some(conn)
        }
    }

    #[inline]
    pub const fn get(&self) -> Option<&Conn> {
        self.conn.as_ref()
    }
    
    #[inline]
    pub fn get_mut(&mut self) -> Option<&mut Conn> {
        self.conn.as_mut()
    }

    #[inline]
    pub fn expire(&mut self) {
        self.conn = None;
    }
}

unsafe impl<Conn: Sync> Sync for LazyConn<Conn> {}
unsafe impl<Conn: Send> Send for LazyConn<Conn> {}

#[doc(hidden)]
#[repr(transparent)]
pub struct LiveConn<'pool, Conn, const SIZE: usize> {
    guard: Guard<'pool, Conn, SIZE>
}

impl<'pool, Conn, const SIZE: usize> LiveConn<'pool, Conn, SIZE> {
    /// # Safety
    ///
    /// The connection MUST be initialized
    #[inline]
    #[must_use]
    pub unsafe fn new(g: Guard<'pool, Conn, SIZE>) -> Self {
        debug_assert!(g.conn.is_some());
        Self { guard: g }
    }
}

impl<'pool, Conn, const SIZE: usize> Deref for LiveConn<'pool, Conn, SIZE> {
    type Target = Conn;

    #[inline]
    fn deref(&self) -> &Self::Target {
        // SAFETY: For a `LiveConn` to exist it must be initialized
        unsafe { self.guard.conn.as_ref().unwrap_unchecked() }
    }
}

impl<'pool, Conn, const SIZE: usize> DerefMut for LiveConn<'pool, Conn, SIZE> {
    #[inline]
    fn deref_mut(&mut self) -> &mut Self::Target {
        // SAFETY: For a `LiveConn` to exist it must be initialized
        unsafe { self.guard.conn.as_mut().unwrap_unchecked() }
    }
}

impl<'pool, Conn, const SIZE: usize> AsRef<Conn> for LiveConn<'pool, Conn, SIZE> {
    #[inline]
    fn as_ref(&self) -> &Conn {
        // SAFETY: For a `LiveConn` to exist it must be initialized
        unsafe { self.guard.conn.as_ref().unwrap_unchecked() } 
    }
}

impl<'pool, Conn, const SIZE: usize> AsMut<Conn> for LiveConn<'pool, Conn, SIZE> {
    #[inline]
    fn as_mut(&mut self) -> &mut Conn {
        // SAFETY: For a `LiveConn` to exist it must be initialized
        unsafe { self.guard.conn.as_mut().unwrap_unchecked() }
    }
}

pub struct Pool<Conn: Connection, const SIZE: usize> {
    pool: LockPool<LazyConn<Conn>, SIZE, MAX_WAITERS>,
    ctx: Conn::Context
}

impl<Conn, const SIZE: usize> Default for Pool<Conn, SIZE>
    where
        Conn: Connection,
        Conn::Context: Default,
{
    fn default() -> Self {
        Self {
            pool: LockPool::new(),
            ctx: Conn::Context::default(),
        }
    }
}

impl<Conn: Connection + Send + Sync, const SIZE: usize> Pool<Conn, SIZE> {
    /// # New Pool
    ///
    /// Create a new pool with no connections initialized. Once a connection is needed it will be
    /// created and stored within the pool.
    ///
    /// # Returns
    ///
    /// A `Pool` instance ready to manage connections. Connections within the pool will be
    /// initialized on demand, based on the pool's usage pattern.
    ///
    /// # Use Case
    ///
    /// Ideal for scenarios where immediate availability of all connections is not necessary, or
    /// where the overhead of initializing all connections upfront is not desirable. This approach
    /// allows the pool to scale its resource allocation based on demand, improving efficiency and
    /// resource utilization.
    ///
    /// # Example
    ///
    /// ```
    /// use swimming::{Pool, Connection};
    ///
    /// struct MyConn;
    /// impl Connection for MyConn {
    ///     type Context = ();
    ///     type Error = ();
    ///
    ///     async fn create(_ctx: &()) -> Result<Self, Self::Error> {
    ///         Ok(MyConn)
    ///     }
    ///
    ///     fn needs_health_check(&mut self, _ctx: &()) -> bool {
    ///         true
    ///     }
    ///
    ///     async fn health_check(&mut self, _ctx: &()) -> Result<(), ()> {
    ///         Ok(())
    ///     }
    /// }
    ///
    /// // Initialize a new pool. Connections will be created as needed.
    /// let pool = Pool::<MyConn, 5>::new(());
    /// ```
    #[must_use] #[inline]
    pub fn new(ctx: Conn::Context) -> Self {
        Self {
            pool: LockPool::new(),
            ctx
        }
    }

    /// # New Eager
    ///
    /// Asynchronously initializes a new `Pool` with each connection eagerly created using a
    /// provided asynchronous function. This method takes a function that returns a future, which
    /// resolves to a connection or an error. Each connection is initialized in sequence, and the
    /// pool is ready to use upon success. If any connection initialization fails, the process is
    /// halted, and an error is returned.
    ///
    /// # Arguments
    ///
    /// * `f` - An asynchronous function which takes the index in the pool as an argument and
    ///         returns a `Result` where `Ok` is the connection (`Conn`) and `Err` is the user
    ///         defined error. This function is called for each connection to be created in the
    ///         pool.
    ///
    /// # Returns
    ///
    /// A `Result` wrapping a new `Pool` instance if all connections are successfully initialized,
    /// or an error of type `E` if any connection initialization fails. This ensures that the pool
    /// is fully operational upon successful creation, with all connections pre-initialized.
    ///
    /// # Errors
    ///
    /// Propagates anything raised by the closure `f`
    ///
    /// # Use Case
    ///
    /// Ideal for scenarios requiring a pool of connections to be fully operational and ready for
    /// use immediately upon instantiation. It allows for the asynchronous and sequential
    /// initialization of connections, ensuring that resources are efficiently allocated and
    /// connections are ready to handle requests without additional setup.
    ///
    /// # Generics
    ///
    /// - `Fut`: A future that resolves to a `Result<Conn, E>`. This future is returned by the
    ///          asynchronous function `f` provided as an argument.
    /// - `F`: The initialization function that takes a pool index (`usize`) and returns a `Fut`.
    /// - `E`: The error type that can be returned by the future. This error type is user-defined
    ///        and must implement `Send + Sync`.
    ///
    /// ```
    /// use swimming::{Pool, Connection};
    /// use core::future::Future;
    ///
    /// struct MyConn;
    ///
    /// impl Connection for MyConn {
    ///     type Context = ();
    ///     type Error = ();
    ///
    ///     async fn create(_ctx: &()) -> Result<Self, Self::Error> {
    ///         Ok(MyConn)
    ///     }
    ///
    ///     fn needs_health_check(&mut self, _ctx: &()) -> bool {
    ///         true
    ///     }
    ///
    ///     async fn health_check(&mut self, _ctx: &()) -> Result<(), ()> {
    ///         Ok(())
    ///     }
    /// }
    ///
    /// fn establish_connection(_index: usize) -> impl Future<Output=Result<MyConn, ()>> {
    ///     // Asynchronously establish and return a new connection
    ///     MyConn::create(&())
    /// }
    ///
    /// async fn example() {
    ///     // Asynchronously create a new pool with pre-initialized connections
    ///     let pool = Pool::<MyConn, 5>::new_eager((), establish_connection)
    ///         .await
    ///         .expect("Failed to create pool");
    /// }
    /// # futures::executor::block_on(example())
    /// ```
    pub async fn new_eager<Fut, F, E>(ctx: Conn::Context, f: F) -> Result<Self, E>
        where 
            Fut: Future<Output = Result<Conn, E>> + Send + Sync,
            F: Fn(usize) -> Fut + Send + Sync,
            E: Send + Sync
    {
        let mut temp: [MaybeUninit<LazyConn<Conn>>; SIZE] = unsafe {
            MaybeUninit::uninit().assume_init()
        };
        for i in 0..SIZE {
            match f(i).await {
                Ok(conn) => {
                    // SAFETY: We are iterating within bounds
                    let slot = unsafe { temp.get_unchecked_mut(i) };
                    slot.write(LazyConn::init(conn))
                },
                Err(err) if core::mem::needs_drop::<Conn>() => {
                    // We came across an error, free all that we have populated and propagate.
                    for needs_drop in &mut temp[0..i] {
                        // SAFETY: iterating only over initialized slots
                        unsafe { needs_drop.assume_init_drop() }
                    }
                    return Err(err);
                }
                Err(err) => return Err(err)
            };
        }
        Ok(Self {
            pool: LockPool::from(unsafe {
                // SAFETY: MaybeUninit has the same size, alignment, and ABI as T
                core::mem::transmute_copy::<[MaybeUninit<LazyConn<Conn>>; SIZE], [LazyConn<Conn>; SIZE]>(&temp)
            }),
            ctx
        })
    }

    /// # Get Connection
    ///
    /// Asynchronously returns a healthy connection from the pool. If all connections are in use,
    /// this method will wait until one becomes available.
    ///
    /// # Returns
    ///
    /// A `Result` wrapping a `LiveConn`, which represents an active and healthy connection from the
    /// pool. The connection is ready for use immediately.
    ///
    /// # Errors
    ///
    /// Returns an error of type `Conn::Error` if there is an issue creating a new connection, in
    /// the case where lazy initialization is triggered because a previously unused slot in the pool
    /// needs to be filled or if the health check failed.
    ///
    /// # Use Case
    ///
    /// This method is essential for obtaining a connection to perform operations. It abstracts away
    /// the details of connection management, including health checks and lazy initialization,
    /// allowing callers to focus on their primary logic without worrying about the underlying
    /// connection state.
    ///
    /// # Example
    ///
    /// ```
    /// # use swimming::{Pool, Connection};
    /// # struct MyConn;
    /// #
    /// # impl Connection for MyConn {
    /// #    type Context = ();
    /// #    type Error = ();
    /// #
    /// #    async fn create(_ctx: &()) -> Result<Self, Self::Error> {
    /// #        Ok(MyConn)
    /// #    }
    /// #
    /// #    fn needs_health_check(&mut self, _ctx: &()) -> bool {
    /// #        true
    /// #    }
    /// #
    /// #    async fn health_check(&mut self, _ctx: &()) -> Result<(), ()> {
    /// #        Ok(())
    /// #    }
    /// # }
    /// type MyPool = Pool<MyConn, 5>;
    ///
    /// async fn use_connection(pool: &MyPool) -> Result<(), <MyConn as Connection>::Error> {
    ///     let _conn = pool.get().await?;
    ///     // do something with the connection... when it goes out of scope it is returned
    ///     // to the pool.
    ///     Ok(())
    /// }
    /// #
    /// # futures::executor::block_on(async {
    /// #     let pool: Pool<MyConn, 5> = Pool::new(());
    /// #     use_connection(&pool).await.unwrap();
    /// # });
    /// ```
    pub async fn get(&self) -> Result<LiveConn<Conn, SIZE>, Conn::Error> {
        let mut maybe_conn = maybe_await!(self.pool.get());
        
        if let Some(conn) = maybe_conn.get_mut() {
            if conn.needs_health_check(&self.ctx) && conn.health_check(&self.ctx).await.is_err() {
                *maybe_conn = LazyConn::init(Conn::create(&self.ctx).await?);
            }
        } else {
            *maybe_conn = LazyConn::init(Conn::create(&self.ctx).await?);
        }

        // SAFETY: The connection must be initialized at this point
        Ok(unsafe { LiveConn::new(maybe_conn) })
    }

    /// # Try to Get Connection
    ///
    /// Attempts to asynchronously retrieve an available connection from the pool without waiting.
    /// If all connections are currently in use, it returns `None`, indicating that no connection
    /// is available immediately. For connections that are available, it performs a health check
    /// if necessary. If the health check fails, it attempts to asynchronously create a new
    /// connection to replace the unhealthy one. This method provides a non-blocking way to acquire
    /// a connection, suitable for use cases where immediate availability is required and waiting
    /// is not an option.
    ///
    /// # Returns
    ///
    /// - `Some(Ok(LiveConn<Conn, SIZE>))`: if a healthy or newly created connection is available.
    /// - `Some(Err(Conn::Error))`: if there was an error during the health check or creating a new
    ///                             connection.
    /// - `None`: if no connections are currently available.
    ///
    /// # Errors
    ///
    /// This method propagates errors encountered when attempting to create a new connection.
    ///
    /// # Use Case
    ///
    /// This method is particularly useful in scenarios where it is preferable to perform another
    /// action or to fallback to an alternative strategy rather than waiting for a connection to
    /// become available. It offers an immediate return path in high-throughput environments where
    /// waiting for resource availability would introduce unacceptable latency. For example,
    /// this method is used within the [`get_or_create`] method.
    ///
    /// # Example
    ///
    /// ```
    /// # use swimming::{Pool, Connection};
    /// # struct MyConn;
    /// #
    /// # impl Connection for MyConn {
    /// #    type Context = ();
    /// #    type Error = ();
    /// #
    /// #    async fn create(_ctx: &()) -> Result<Self, Self::Error> {
    /// #        Ok(MyConn)
    /// #    }
    /// #
    /// #    fn needs_health_check(&mut self, _ctx: &()) -> bool {
    /// #        true
    /// #    }
    /// #
    /// #    async fn health_check(&mut self, _ctx: &()) -> Result<(), ()> {
    /// #        Ok(())
    /// #    }
    /// # }
    /// type MyPool = Pool<MyConn, 5>;
    ///
    /// async fn maybe_use_connection(pool: &MyPool) -> Result<(), <MyConn as Connection>::Error> {
    ///     let Some(maybe_conn) = pool.try_get().await else {
    ///         panic!("There were 5 connections available.");
    ///     };
    ///     let _conn = maybe_conn?;
    ///     Ok(())
    /// }
    /// #
    /// # futures::executor::block_on(async {
    /// #     let pool: Pool<MyConn, 5> = Pool::new(());
    /// #     maybe_use_connection(&pool).await.unwrap();
    /// # });
    /// ```
    ///
    /// [`get_or_create`]: Pool::get_or_create
    pub async fn try_get(&self) -> Option<Result<LiveConn<Conn, SIZE>, Conn::Error>> {
        let mut maybe_conn = self.pool.try_get()?;

        if let Some(conn) = maybe_conn.get_mut() {
            if conn.needs_health_check(&self.ctx) && conn.health_check(&self.ctx).await.is_err() {
                match Conn::create(&self.ctx).await {
                    Ok(conn) => *maybe_conn = LazyConn::init(conn),
                    Err(e) => return Some(Err(e))
                };
            }
        } else {
            match Conn::create(&self.ctx).await {
                Ok(conn) => *maybe_conn = LazyConn::init(conn),
                Err(e) => return Some(Err(e))
            }
        }

        // SAFETY: The connection must be initialized at this point
        Some(Ok(unsafe { LiveConn::new(maybe_conn) }))
    }

    /// # Get or Create Connection
    ///
    /// Attempts to asynchronously retrieve an available connection from the pool without waiting.
    /// If all connections are currently in use, it directly creates a new connection instead of
    /// returning `None`. This method combines the fail-fast retrieval of `try_get` with an
    /// immediate fallback to connection creation, ensuring that a connection is always provided
    /// when possible, either by reusing an existing one or by creating a fresh one.
    ///
    /// # Returns
    ///
    /// - `Ok(FreshOrPooled::Pooled(connection))`: if a connection from the pool is available or has
    ///                                            been successfully health-checked and is ready for
    ///                                            use.
    /// - `Ok(FreshOrPooled::Fresh(connection))`: if a new connection has been created because the
    ///                                           pool was depleted or an available connection
    ///                                           failed its health check.
    /// - `Err(Conn::Error)`: if there was an error during the health check of an available
    ///                       connection or in creating a new connection.
    ///
    /// # Errors
    ///
    /// This method propagates any errors encountered during the creation of a new connection.
    /// These errors are encapsulated in the `Conn::Error` type, allowing for consistent error
    /// handling regardless of the connection's source.
    ///
    /// # Use Case
    ///
    /// `get_or_create` is ideal for scenarios requiring guaranteed connection availability without
    /// the overhead of waiting for a connection to be released back into the pool. It is especially
    /// useful in high-demand environments where the cost of creating a new connection is outweighed
    /// by the need for immediate access to a connection.
    ///
    /// # Example
    ///
    /// ```
    /// # use swimming::{Pool, Connection, FreshOrPooled};
    /// # struct MyConn;
    /// #
    /// # impl Connection for MyConn {
    /// #    type Context = ();
    /// #    type Error = ();
    /// #
    /// #    async fn create(_ctx: &()) -> Result<Self, Self::Error> {
    /// #        Ok(MyConn)
    /// #    }
    /// #
    /// #    fn needs_health_check(&mut self, _ctx: &()) -> bool {
    /// #        false
    /// #    }
    /// #
    /// #    async fn health_check(&mut self, _ctx: &()) -> Result<(), ()> {
    /// #        Ok(())
    /// #    }
    /// # }
    /// type MyPool = Pool<MyConn, 5>;
    ///
    /// async fn use_connection(pool: &MyPool) -> Result<(), <MyConn as Connection>::Error> {
    ///     let _conn = pool.get_or_create().await?;
    ///     // FreshOrPooled dereferences into the connection, so we can use the connection
    ///     // how we would normally...
    ///     Ok(())
    /// }
    /// #
    /// # futures::executor::block_on(async {
    /// #     let pool: Pool<MyConn, 5> = Pool::new(());
    /// #     use_connection(&pool).await.unwrap();
    /// # });
    /// ```
    #[inline]
    pub async fn get_or_create(&self) -> Result<FreshOrPooled<Conn, SIZE>, Conn::Error> {
        match self.try_get().await {
            Some(res) => res.map(FreshOrPooled::Pooled),
            None => Conn::create(&self.ctx).await.map(FreshOrPooled::Fresh)
        }
    }
}

/// `FreshOrPooled` Enum
///
/// Represents a connection obtained from the pool, distinguishing between freshly created
/// connections and those reused from the pool. This abstraction facilitates seamless operation,
/// whether the connection is new or pooled, by abstracting away the details of connection
/// management.
///
/// # Variants
///
/// - `Fresh(Conn)`: A newly created connection that was not obtained from the pool. This variant is
///                  used when the pool is depleted.
///
/// - `Pooled(LiveConn<'pool, Conn, SIZE>)`: A connection retrieved from the pool, ready for use.
///
/// # Dereferencing
///
/// `FreshOrPooled` implements both `Deref` and `DerefMut` traits, allowing direct access to the
/// underlying connection regardless of its variant. This design enables users to interact with
/// the connection transparently, without concerning themselves with its fresh or pooled nature.
///
/// It is advisable to dereference only once at the beginning of an operation to minimize
/// unnecessary branching. While the performance impact of repeated dereferencing is minimal due to
/// predictable branching and modern CPU branch prediction capabilities.
pub enum FreshOrPooled<'pool, Conn, const SIZE: usize> {
    Fresh(Conn),
    Pooled(LiveConn<'pool, Conn, SIZE>)
}

macro_rules! match_fresh_or_pooled {
    ($e:expr, |$c:ident| $arm:expr) => {
        match $e {
            $crate::FreshOrPooled::Fresh($c) => $arm,
            $crate::FreshOrPooled::Pooled($c) => $arm
        }
    };
}

impl<'pool, Conn, const SIZE: usize> Deref for FreshOrPooled<'pool, Conn, SIZE> {
    type Target = Conn;

    #[inline]
    fn deref(&self) -> &Self::Target {
        match_fresh_or_pooled!(self, |conn| conn)
    }
}

impl<'pool, Conn, const SIZE: usize> DerefMut for FreshOrPooled<'pool, Conn, SIZE> {
    #[inline]
    fn deref_mut(&mut self) -> &mut Self::Target {
        match_fresh_or_pooled!(self, |conn| conn)
    }
}

impl<'pool, Conn, const SIZE: usize> FreshOrPooled<'pool, Conn, SIZE> {
    /// Returns `true` if the connection is from the pool
    #[inline]
    pub const fn is_pooled(&self) -> bool {
        matches!(self, Self::Pooled(_))
    }

    /// Returns `true` if the connection is not from the pool
    #[inline]
    pub const fn is_fresh(&self) -> bool {
        matches!(self, Self::Fresh(_))
    }
}

#[allow(clippy::nursery)]
#[cfg(test)]
mod tests {
    use super::*;
    extern crate alloc;
    use alloc::string::String;
    use futures::executor::block_on;

    trait Inner {
        fn new() -> Self;
    }

    struct Conn<T: Inner> {
        inner: T,
        needs_health_check: bool,
        is_healthy: bool
    }

    impl<T: Inner> Conn<T> {
        fn new() -> Self {
            Self {
                inner: T::new(),
                needs_health_check: true,
                is_healthy: true
            }
        }
    }

    unsafe impl<T: Inner> Send for Conn<T> {}
    unsafe impl<T: Inner> Sync for Conn<T> {}

    impl<T: Inner> Connection for Conn<T> {
        type Context = ();
        type Error = ();

        async fn create(_ctx: &()) -> Result<Self, Self::Error> {
            Ok(Self::new())
        }

        fn needs_health_check(&mut self, _ctx: &()) -> bool {
            self.needs_health_check
        }

        async fn health_check(&mut self, _ctx: &()) -> Result<(), Self::Error> {
            if self.is_healthy {
                Ok(())
            } else {
                Err(())
            }
        }
    }

    impl Inner for String {
        fn new() -> Self {
            String::from("Hello World")
        }
    }

    impl Inner for usize {
        fn new() -> Self {
            7
        }
    }

    fn create_conn<C: Connection<Context = ()>>(_idx: usize) -> impl Future<Output = Result<C, <C as Connection>::Error>> {
        C::create(&())
    }

    macro_rules! create_conn_fail {
        ($fail_on:literal, $c:ty) => {
            |idx| async move {
                if idx == $fail_on {
                    Err(())
                } else {
                    <$c>::create(&()).await
                }
            }
        };
    }

    macro_rules! assert_string_inner {
        ($live_conn:ident) => {
            assert_eq!($live_conn.inner, String::from("Hello World"))
        };
    }

    macro_rules! assert_usize_inner {
        ($live_conn:ident) => {
            assert_eq!($live_conn.inner, 7)
        };
    }

    #[test]
    fn new_eager_success_alloc() {
        let fut = async {
            let pool: Pool<Conn<String>, 5> = Pool::new_eager((), create_conn)
                .await
                .unwrap();

            let c0 = pool.get().await.unwrap();
            assert_string_inner!(c0);
            let c1 = pool.get().await.unwrap();
            assert_string_inner!(c1);
            let c2 = pool.get().await.unwrap();
            assert_string_inner!(c2);
            let c3 = pool.get().await.unwrap();
            assert_string_inner!(c3);
            let c4 = pool.get().await.unwrap();
            assert_string_inner!(c4);
        };

        block_on(fut);
    }

    #[test]
    fn new_eager_failure_alloc() {
        // ==123447== LEAK SUMMARY:
        // ==123447==    definitely lost: 0 bytes in 0 blocks
        // ==123447==    indirectly lost: 0 bytes in 0 blocks
        // ==123447==      possibly lost: 0 bytes in 0 blocks
        let fut = async {
            assert!(
                Pool::<Conn<String>, 5>::new_eager((), create_conn_fail!(3, Conn<String>))
                    .await
                    .is_err()
            );
        };

        block_on(fut);
    }

    #[test]
    fn new_eager_success() {
        let fut = async {
            let pool: Pool<Conn<usize>, 5> = Pool::new_eager((), create_conn)
                .await
                .unwrap();

            let c0 = pool.get().await.unwrap();
            assert_usize_inner!(c0);
            let c1 = pool.get().await.unwrap();
            assert_usize_inner!(c1);
            let c2 = pool.get().await.unwrap();
            assert_usize_inner!(c2);
            let c3 = pool.get().await.unwrap();
            assert_usize_inner!(c3);
            let c4 = pool.get().await.unwrap();
            assert_usize_inner!(c4);
        };

        block_on(fut);
    }

    #[test]
    fn new_eager_failure() {
        let fut = async {
            assert!(
                Pool::<Conn<usize>, 5>::new_eager((), create_conn_fail!(3, Conn<usize>))
                    .await
                    .is_err()
            );
        };

        block_on(fut);
    }

    #[test]
    fn get_health_check() {
        let fut = async {
            let pool: Pool<Conn<usize>, 5> = Pool::new_eager((), create_conn)
                .await
                .unwrap();

            let mut g0 = pool.get().await.unwrap();
            g0.needs_health_check = true;
            drop(g0);

            let mut g0 = pool.get().await.unwrap();
            g0.is_healthy = false;
            drop(g0);

            let g0 = pool.get().await.unwrap();
            drop(g0);
        };

        block_on(fut);
    }

    #[test]
    fn try_get_health_check() {
        let fut = async {
            let pool: Pool<Conn<usize>, 5> = Pool::new_eager((), create_conn)
                .await
                .unwrap();

            let mut g0 = pool.try_get().await.unwrap().unwrap();
            g0.needs_health_check = true;
            drop(g0);

            let mut g0 = pool.try_get().await.unwrap().unwrap();
            g0.is_healthy = false;
            drop(g0);

            let g0 = pool.try_get().await.unwrap().unwrap();
            drop(g0);
        };

        block_on(fut);
    }

    #[test]
    fn get_uninitialized() {
        let fut = async {
            let pool: Pool<Conn<usize>, 5> = Pool::new(());

            let c0 = pool.get().await.unwrap();
            assert_usize_inner!(c0);
            let c1 = pool.get().await.unwrap();
            assert_usize_inner!(c1);
            let c2 = pool.get().await.unwrap();
            assert_usize_inner!(c2);
            let c3 = pool.get().await.unwrap();
            assert_usize_inner!(c3);
            let c4 = pool.get().await.unwrap();
            assert_usize_inner!(c4);
        };

        block_on(fut);
    }

    #[test]
    fn try_get_uninitialized() {
        let fut = async {
            let pool: Pool<Conn<usize>, 5> = Pool::new(());

            let c0 = pool.try_get().await.unwrap().unwrap();
            assert_usize_inner!(c0);
            let c1 = pool.try_get().await.unwrap().unwrap();
            assert_usize_inner!(c1);
            let c2 = pool.try_get().await.unwrap().unwrap();
            assert_usize_inner!(c2);
            let c3 = pool.try_get().await.unwrap().unwrap();
            assert_usize_inner!(c3);
            let c4 = pool.try_get().await.unwrap().unwrap();
            assert_usize_inner!(c4);
        };

        block_on(fut);
    }

    #[test]
    fn get_or_create() {
        let fut = async {
            let pool: Pool<Conn<usize>, 1> = Pool::default();

            let pooled = pool.get_or_create().await.unwrap();
            assert_usize_inner!(pooled);
            assert!(pooled.is_pooled());

            let fresh = pool.get_or_create().await.unwrap();
            assert_usize_inner!(pooled);
            assert!(fresh.is_fresh());

            drop(pooled);

            let pooled = pool.get_or_create().await.unwrap();
            assert_usize_inner!(pooled);
            assert!(pooled.is_pooled());

            drop(fresh);

            let fresh = pool.get_or_create().await.unwrap();
            assert_usize_inner!(fresh);
            assert!(fresh.is_fresh());
        };

        block_on(fut);
    }

    #[test]
    fn lazy_conn_basic_utils() {
        let conn = LazyConn::<Conn<usize>>::default();
        assert!(conn.get().is_none());

        let mut conn = LazyConn::init(Conn::<usize>::new());
        assert!(conn.get().is_some());
        conn.expire();
        assert!(conn.get().is_none());
    }
}