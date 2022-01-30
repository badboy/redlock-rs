//! redlock-rs is an implementation of the [distributed locking
//! mechanism](http://redis.io/topics/distlock) built on top of Redis.
//!
//! It is more or less a port of the [Ruby version](https://github.com/antirez/redlock-rb).
//!
//! # Basic Operation
//! ```rust,no_run
//! # use redlock::RedLock;
//! let rl = RedLock::new(vec![
//!     "redis://127.0.0.1:6380/",
//!     "redis://127.0.0.1:6381/",
//!     "redis://127.0.0.1:6382/"]);
//!
//! let lock;
//! loop {
//!   match rl.lock("mutex".as_bytes(), 1000) {
//!     Ok(Some(l)) => { lock = l; break },
//!     Ok(None) => (),
//!     Err(e) => panic!("Error communicating with redis: {}", e)
//!   }
//! }
//!
//! // Critical section
//!
//! rl.unlock(&lock);
//! ```

mod redlock;

pub use crate::redlock::{Lock, RedLock};
