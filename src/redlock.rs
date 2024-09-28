use std::fs::File;
use std::io::{self, Read};
use std::thread::sleep;
use std::time::{Duration, Instant};

use rand::{thread_rng, Rng};
use redis::Value::Okay;
use redis::{Client, IntoConnectionInfo, RedisError, RedisResult, Value};

const DEFAULT_RETRY_COUNT: u32 = 3;
const DEFAULT_RETRY_DELAY: u32 = 200;
const CLOCK_DRIFT_FACTOR: f32 = 0.01;
const UNLOCK_SCRIPT: &str = r"if redis.call('get',KEYS[1]) == ARGV[1] then
                                return redis.call('del',KEYS[1])
                              else
                                return 0
                              end";

/// The lock manager.
///
/// Implements the necessary functionality to acquire and release locks
/// and handles the Redis connections.
#[derive(Debug, Clone)]
pub struct RedLock {
    /// List of all Redis clients
    pub servers: Vec<Client>,
    quorum: u32,
    retry_count: u32,
    retry_delay: u32,
}

pub struct Lock<'a> {
    /// The resource to lock. Will be used as the key in Redis.
    pub resource: Vec<u8>,
    /// The value for this lock.
    pub val: Vec<u8>,
    /// Time the lock is still valid.
    /// Should only be slightly smaller than the requested TTL.
    pub validity_time: usize,
    /// Used to limit the lifetime of a lock to its lock manager.
    pub lock_manager: &'a RedLock,
}

pub struct RedLockGuard<'a> {
    pub lock: Lock<'a>,
}

impl Drop for RedLockGuard<'_> {
    fn drop(&mut self) {
        self.lock.lock_manager.unlock(&self.lock);
    }
}

impl RedLock {
    /// Create a new lock manager instance, defined by the given Redis connection uris.
    /// Quorum is defined to be N/2+1, with N being the number of given Redis instances.
    ///
    /// Sample URI: `"redis://127.0.0.1:6379"`
    pub fn new<T: AsRef<str> + IntoConnectionInfo>(uris: Vec<T>) -> RedLock {
        let servers: Vec<Client> = uris
            .into_iter()
            .map(|uri| Client::open(uri).unwrap())
            .collect();

        Self::with_clients(servers)
    }

    pub fn with_client(server: Client) -> RedLock {
        Self::with_clients(vec![server])
    }

    /// Create a new lock manager instance, defined by the given Redis client instance.
    /// Quorum is defined to be N/2+1, with N being the number of given client instances.
    pub fn with_clients(servers: Vec<Client>) -> RedLock {
        let quorum = (servers.len() as u32) / 2 + 1;

        RedLock {
            servers,
            quorum,
            retry_count: DEFAULT_RETRY_COUNT,
            retry_delay: DEFAULT_RETRY_DELAY,
        }
    }

    /// Get 20 random bytes from `/dev/urandom`.
    pub fn get_unique_lock_id(&self) -> io::Result<Vec<u8>> {
        let file = File::open("/dev/urandom")?;
        let mut buf = Vec::with_capacity(20);
        match file.take(20).read_to_end(&mut buf) {
            Ok(20) => Ok(buf),
            Ok(_containers) => Err(io::Error::new(
                io::ErrorKind::Other,
                "Can't read enough random bytes",
            )),
            Err(e) => Err(e),
        }
    }

    /// Set retry count and retry delay.
    ///
    /// Retry count defaults to `3`.
    /// Retry delay defaults to `200`.
    pub fn set_retry(&mut self, count: u32, delay: u32) {
        self.retry_count = count;
        self.retry_delay = delay;
    }

    fn lock_instance(
        &self,
        client: &redis::Client,
        resource: &[u8],
        val: &[u8],
        ttl: usize,
    ) -> Result<bool, RedisError> {
        client
            .get_connection()
            .and_then(|mut conn| {
                redis::cmd("SET")
                    .arg(resource)
                    .arg(val)
                    .arg("nx")
                    .arg("px")
                    .arg(ttl)
                    .query::<Value>(&mut conn)
            })
            .map(|result| matches!(result, Okay))
    }

    /// Acquire the lock for the given resource and the requested TTL.
    ///
    /// If it succeeds, a `Lock` instance is returned,
    /// including the value and the validity time
    ///
    /// `Err(RedisError)` is returned on any Redis error, `None` is returned if the lock could
    /// not be acquired and the user should retry
    pub fn lock(&self, resource: &[u8], ttl: usize) -> Result<Option<Lock>, RedisError> {
        let val = self.get_unique_lock_id().unwrap();

        let mut rng = thread_rng();

        for _ in 0..self.retry_count {
            let mut n = 0;
            let start_time = Instant::now();
            for client in &self.servers {
                if self.lock_instance(client, resource, &val, ttl)? {
                    n += 1;
                }
            }

            let drift = (ttl as f32 * CLOCK_DRIFT_FACTOR) as usize + 2;
            let elapsed = start_time.elapsed();
            let validity_time = ttl
                - drift
                - elapsed.as_secs() as usize * 1000
                - elapsed.subsec_nanos() as usize / 1_000_000;

            if n >= self.quorum && validity_time > 0 {
                return Ok(Some(Lock {
                    lock_manager: self,
                    resource: resource.to_vec(),
                    val,
                    validity_time,
                }));
            } else {
                for client in &self.servers {
                    self.unlock_instance(client, resource, &val);
                }
            }

            let n = rng.gen_range(0..self.retry_delay);
            sleep(Duration::from_millis(u64::from(n)));
        }
        Ok(None)
    }

    /// Acquire the lock for the given resource and the requested TTL. \
    /// Will wait and yield current task (tokio runtime) until the lock \
    /// is acquired
    ///
    /// Returns a `RedLockGuard` instance which is a RAII wrapper for \
    /// the old `Lock` object
    #[cfg(feature = "async")]
    pub async fn acquire_async(
        &self,
        resource: &[u8],
        ttl: usize,
    ) -> Result<RedLockGuard<'_>, RedisError> {
        let lock;
        loop {
            match self.lock(resource, ttl)? {
                Some(l) => {
                    lock = l;
                    break;
                }
                None => tokio::task::yield_now().await,
            }
        }
        Ok(RedLockGuard { lock })
    }

    pub fn acquire(&self, resource: &[u8], ttl: usize) -> Result<RedLockGuard<'_>, RedisError> {
        let lock;
        loop {
            if let Some(l) = self.lock(resource, ttl)? {
                lock = l;
                break;
            }
        }
        Ok(RedLockGuard { lock })
    }

    fn unlock_instance(&self, client: &redis::Client, resource: &[u8], val: &[u8]) -> bool {
        let mut con = match client.get_connection() {
            Err(_containers) => return false,
            Ok(val) => val,
        };
        let script = redis::Script::new(UNLOCK_SCRIPT);
        let result: RedisResult<i32> = script.key(resource).arg(val).invoke(&mut con);
        match result {
            Ok(val) => val == 1,
            Err(_containers) => false,
        }
    }

    /// Unlock the given lock.
    ///
    /// Unlock is best effort. It will simply try to contact all instances
    /// and remove the key.
    pub fn unlock(&self, lock: &Lock) {
        for client in &self.servers {
            self.unlock_instance(client, &lock.resource, &lock.val);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use anyhow::Result;
    use testcontainers::core::IntoContainerPort;
    use testcontainers::runners::AsyncRunner;
    use testcontainers::{ContainerAsync, GenericImage};

    async fn init() -> Result<(Option<Vec<ContainerAsync<GenericImage>>>, Vec<String>)> {
        match std::env::var("ADDRESSES") {
            Ok(addresses) => Ok((None, addresses.split(',').map(String::from).collect())),
            _ => {
                let (containers, addresses) = start_containers().await?;
                Ok((Some(containers), addresses))
            }
        }
    }

    async fn start_containers() -> Result<(Vec<ContainerAsync<GenericImage>>, Vec<String>)> {
        let mut containers = Vec::new();
        let mut addresses = Vec::new();
        for _ in 0..3 {
            let container = GenericImage::new("redis", "7-alpine")
                .with_exposed_port(6379.tcp())
                .start()
                .await?;
            let address = format!(
                "redis://localhost:{}",
                container.get_host_port_ipv4(6379).await?
            );
            containers.push(container);
            addresses.push(address);
        }
        Ok((containers, addresses))
    }

    #[test]
    fn test_redlock_get_unique_id() -> Result<()> {
        let rl = RedLock::new(Vec::<String>::new());
        assert_eq!(rl.get_unique_lock_id()?.len(), 20);
        Ok(())
    }

    #[test]
    fn test_redlock_get_unique_id_uniqueness() -> Result<()> {
        let rl = RedLock::new(Vec::<String>::new());

        let id1 = rl.get_unique_lock_id()?;
        let id2 = rl.get_unique_lock_id()?;

        assert_eq!(20, id1.len());
        assert_eq!(20, id2.len());
        assert_ne!(id1, id2);
        Ok(())
    }

    #[tokio::test]
    async fn test_redlock_valid_instance() -> Result<()> {
        let (_containers, addresses) = init().await?;
        let rl = RedLock::new(addresses);
        assert_eq!(3, rl.servers.len());
        assert_eq!(2, rl.quorum);
        Ok(())
    }

    #[tokio::test]
    async fn test_redlock_direct_unlock_fails() -> Result<()> {
        let (_containers, addresses) = init().await?;
        let rl = RedLock::new(addresses);
        let key = rl.get_unique_lock_id()?;

        let val = rl.get_unique_lock_id()?;
        assert!(!rl.unlock_instance(&rl.servers[0], &key, &val));
        Ok(())
    }

    #[tokio::test]
    async fn test_redlock_direct_unlock_succeeds() -> Result<()> {
        let (_containers, addresses) = init().await?;
        let rl = RedLock::new(addresses);
        let key = rl.get_unique_lock_id()?;

        let val = rl.get_unique_lock_id()?;
        let mut con = rl.servers[0].get_connection()?;
        redis::cmd("SET").arg(&key).arg(&val).exec(&mut con)?;

        assert!(rl.unlock_instance(&rl.servers[0], &key, &val));
        Ok(())
    }

    #[tokio::test]
    async fn test_redlock_direct_lock_succeeds() -> Result<()> {
        let (_containerscontainers, addresses) = init().await?;
        let rl = RedLock::new(addresses);
        let key = rl.get_unique_lock_id()?;

        let val = rl.get_unique_lock_id()?;
        let mut con = rl.servers[0].get_connection()?;

        redis::cmd("DEL").arg(&key).exec(&mut con)?;
        assert!(rl.lock_instance(&rl.servers[0], &key, &val, 1000)?);
        Ok(())
    }

    #[tokio::test]
    async fn test_redlock_unlock() -> Result<()> {
        let (_containers, addresses) = init().await?;
        let rl = RedLock::new(addresses);
        let key = rl.get_unique_lock_id()?;

        let val = rl.get_unique_lock_id()?;
        let mut con = rl.servers[0].get_connection()?;
        let _: () = redis::cmd("SET")
            .arg(&key)
            .arg(&val)
            .query(&mut con)
            .unwrap();

        let lock = Lock {
            lock_manager: &rl,
            resource: key,
            val,
            validity_time: 0,
        };
        rl.unlock(&lock);
        Ok(())
    }

    #[tokio::test]
    async fn test_redlock_lock() -> Result<()> {
        let (_containers, addresses) = init().await?;
        let rl = RedLock::new(addresses);

        let key = rl.get_unique_lock_id()?;
        match rl.lock(&key, 1000)? {
            Some(lock) => {
                assert_eq!(key, lock.resource);
                assert_eq!(20, lock.val.len());
                assert!(lock.validity_time > 900);
                assert!(
                    lock.validity_time > 900,
                    "validity time: {}",
                    lock.validity_time
                );
            }
            None => panic!("Lock failed"),
        }
        Ok(())
    }

    #[tokio::test]
    async fn test_redlock_lock_unlock() -> Result<()> {
        let (_containers, addresses) = init().await?;
        let rl = RedLock::new(addresses.to_owned());
        let rl2 = RedLock::new(addresses);

        let key = rl.get_unique_lock_id()?;

        let lock = rl.lock(&key, 1000)?.unwrap();
        assert!(
            lock.validity_time > 900,
            "validity time: {}",
            lock.validity_time
        );

        if let Some(_containersl) = rl2.lock(&key, 1000)? {
            panic!("Lock acquired, even though it should be locked")
        }

        rl.unlock(&lock);

        match rl2.lock(&key, 1000)? {
            Some(l) => assert!(l.validity_time > 900),
            None => panic!("Lock couldn't be acquired"),
        }
        Ok(())
    }

    #[tokio::test]
    async fn test_redlock_lock_unlock_raii() -> Result<()> {
        let (_containers, addresses) = init().await?;
        let rl = RedLock::new(addresses.to_owned());
        let rl2 = RedLock::new(addresses);

        let key = rl.get_unique_lock_id()?;
        {
            let lock_guard = rl.acquire(&key, 1000)?;
            let lock = &lock_guard.lock;
            assert!(
                lock.validity_time > 900,
                "validity time: {}",
                lock.validity_time
            );

            if let Some(_containersl) = rl2.lock(&key, 1000)? {
                panic!("Lock acquired, even though it should be locked")
            }
        }

        match rl2.lock(&key, 1000)? {
            Some(l) => assert!(l.validity_time > 900),
            None => panic!("Lock couldn't be acquired"),
        }
        Ok(())
    }

    #[test]
    fn test_redlock_lock_error() -> Result<()> {
        let rl = RedLock::new(vec!["redis://nonexistent"]);
        let key = rl.get_unique_lock_id()?;
        match rl.lock(&key, 1000) {
            Ok(_containers) => panic!("Expected error"),
            Err(e) => assert!(e.is_io_error()),
        }
        Ok(())
    }
}
