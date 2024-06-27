use apalis_core::codec::json::JsonCodec;
use apalis_core::data::Extensions;
use apalis_core::error::Error;
use apalis_core::layers::{Ack, AckLayer};
use apalis_core::poller::controller::Controller;
use apalis_core::poller::stream::BackendStream;
use apalis_core::poller::Poller;
use apalis_core::request::{Request, RequestStream};
use apalis_core::storage::Storage;
use apalis_core::task::attempt::Attempt;
use apalis_core::task::namespace::Namespace;
use apalis_core::task::task_id::TaskId;
use apalis_core::worker::WorkerId;
use apalis_core::{Backend, Codec};
use async_stream::try_stream;
use chrono::Utc;
use futures::{FutureExt, TryFutureExt, TryStreamExt};
use log::*;
use redis::ErrorKind;
use redis::{aio::ConnectionManager, Client, IntoConnectionInfo, RedisError, Script, Value};
use serde::{de::DeserializeOwned, Deserialize, Serialize};
use std::fmt;
use std::num::TryFromIntError;
use std::sync::Arc;
use std::{marker::PhantomData, time::Duration};

/// Shorthand to create a client and connect
pub async fn connect<S: IntoConnectionInfo>(redis: S) -> Result<ConnectionManager, RedisError> {
    let client = Client::open(redis.into_connection_info()?)?;
    let conn = client.get_connection_manager().await?;
    Ok(conn)
}

const ACTIVE_JOBS_LIST: &str = "{queue}:active";
const CONSUMERS_SET: &str = "{queue}:consumers";
const DEAD_JOBS_SET: &str = "{queue}:dead";
const DONE_JOBS_SET: &str = "{queue}:done";
const FAILED_JOBS_SET: &str = "{queue}:failed";
const INFLIGHT_JOB_SET: &str = "{queue}:inflight";
const JOB_DATA_HASH: &str = "{queue}:data";
const SCHEDULED_JOBS_SET: &str = "{queue}:scheduled";
const SIGNAL_LIST: &str = "{queue}:signal";

/// Represents redis key names for various components of the RedisStorage.
///
/// This struct defines keys used in Redis to manage jobs and their lifecycle in the storage.
#[derive(Clone, Debug)]
pub struct RedisQueueInfo {
    /// Key for the list of currently active jobs.
    pub active_jobs_list: String,

    /// Key for the set of active consumers.
    pub consumers_set: String,

    /// Key for the set of jobs that are no longer retryable.
    pub dead_jobs_set: String,

    /// Key for the set of jobs that have completed successfully.
    pub done_jobs_set: String,

    /// Key for the set of jobs that have failed.
    pub failed_jobs_set: String,

    /// Key for the set of jobs that are currently being processed.
    pub inflight_jobs_set: String,

    /// Key for the hash storing data for each job.
    pub job_data_hash: String,

    /// Key for the set of jobs scheduled for future execution.
    pub scheduled_jobs_set: String,

    /// Key for the list used for signaling and communication between consumers and producers.
    pub signal_list: String,
}

#[derive(Clone, Debug)]
struct RedisScript {
    ack_job: Script,
    enqueue_scheduled: Script,
    get_jobs: Script,
    kill_job: Script,
    push_job: Script,
    reenqueue_active: Script,
    reenqueue_orphaned: Script,
    register_consumer: Script,
    retry_job: Script,
    schedule_job: Script,
    vacuum: Script,
}

/// The actual structure of a Redis job
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RedisJob<J> {
    ctx: Context,
    job: J,
}

impl<T> From<RedisJob<T>> for Request<T> {
    fn from(val: RedisJob<T>) -> Self {
        let mut data = Extensions::new();
        data.insert(val.ctx.id.clone());
        data.insert(Attempt::new_with_value(val.ctx.attempts));
        data.insert(val.ctx);
        Request::new_with_data(val.job, data)
    }
}

impl<T> TryFrom<Request<T>> for RedisJob<T> {
    type Error = RedisError;
    fn try_from(val: Request<T>) -> Result<Self, Self::Error> {
        let task_id = val
            .get::<TaskId>()
            .cloned()
            .ok_or((ErrorKind::IoError, "Missing TaskId"))?;
        let attempts = val.get::<Attempt>().cloned().unwrap_or_default();
        Ok(RedisJob {
            job: val.take(),
            ctx: Context {
                attempts: attempts.current(),
                id: task_id,
            },
        })
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct Context {
    id: TaskId,
    attempts: usize,
}

/// Config for a [RedisStorage]
#[derive(Clone, Debug)]
pub struct Config {
    fetch_interval: Duration,
    buffer_size: usize,
    max_retries: usize,
    keep_alive: Duration,
    enqueue_scheduled: Duration,
    namespace: String,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            fetch_interval: Duration::from_millis(100),
            buffer_size: 10,
            max_retries: 5,
            keep_alive: Duration::from_secs(30),
            enqueue_scheduled: Duration::from_secs(30),
            namespace: String::from("apalis::redis"),
        }
    }
}

impl Config {
    /// Get the rate of polling per unit of time
    pub fn get_fetch_interval(&self) -> &Duration {
        &self.fetch_interval
    }

    /// Get the number of jobs to fetch
    pub fn get_buffer_size(&self) -> usize {
        self.buffer_size
    }

    /// Get the max retries
    pub fn get_max_retries(&self) -> usize {
        self.max_retries
    }

    /// get the keep live rate
    pub fn get_keep_alive(&self) -> &Duration {
        &self.keep_alive
    }

    /// get the enqueued setting
    pub fn get_enqueue_scheduled(&self) -> &Duration {
        &self.enqueue_scheduled
    }

    /// get the namespace
    pub fn get_namespace(&self) -> &String {
        &self.namespace
    }

    /// get the fetch interval
    pub fn set_fetch_interval(mut self, fetch_interval: Duration) -> Self {
        self.fetch_interval = fetch_interval;
        self
    }

    /// set the buffer setting
    pub fn set_buffer_size(mut self, buffer_size: usize) -> Self {
        self.buffer_size = buffer_size;
        self
    }

    /// set the max-retries setting
    pub fn set_max_retries(mut self, max_retries: usize) -> Self {
        self.max_retries = max_retries;
        self
    }

    /// set the keep-alive setting
    pub fn set_keep_alive(mut self, keep_alive: Duration) -> Self {
        self.keep_alive = keep_alive;
        self
    }

    /// get the enqueued setting
    pub fn set_enqueue_scheduled(mut self, enqueue_scheduled: Duration) -> Self {
        self.enqueue_scheduled = enqueue_scheduled;
        self
    }

    /// set the namespace for the Storage
    pub fn set_namespace(mut self, namespace: &str) -> Self {
        self.namespace = namespace.to_owned();
        self
    }

    /// Returns the Redis key for the list of active jobs associated with the queue.
    /// The key is dynamically generated using the namespace of the queue.
    ///
    /// # Returns
    /// A `String` representing the Redis key for the active jobs list.
    pub fn active_jobs_list(&self) -> String {
        ACTIVE_JOBS_LIST.replace("{queue}", &self.namespace)
    }

    /// Returns the Redis key for the set of consumers associated with the queue.
    /// The key is dynamically generated using the namespace of the queue.
    ///
    /// # Returns
    /// A `String` representing the Redis key for the consumers set.
    pub fn consumers_set(&self) -> String {
        CONSUMERS_SET.replace("{queue}", &self.namespace)
    }

    /// Returns the Redis key for the set of dead jobs associated with the queue.
    /// The key is dynamically generated using the namespace of the queue.
    ///
    /// # Returns
    /// A `String` representing the Redis key for the dead jobs set.
    pub fn dead_jobs_set(&self) -> String {
        DEAD_JOBS_SET.replace("{queue}", &self.namespace)
    }

    /// Returns the Redis key for the set of done jobs associated with the queue.
    /// The key is dynamically generated using the namespace of the queue.
    ///
    /// # Returns
    /// A `String` representing the Redis key for the done jobs set.
    pub fn done_jobs_set(&self) -> String {
        DONE_JOBS_SET.replace("{queue}", &self.namespace)
    }

    /// Returns the Redis key for the set of failed jobs associated with the queue.
    /// The key is dynamically generated using the namespace of the queue.
    ///
    /// # Returns
    /// A `String` representing the Redis key for the failed jobs set.
    pub fn failed_jobs_set(&self) -> String {
        FAILED_JOBS_SET.replace("{queue}", &self.namespace)
    }

    /// Returns the Redis key for the set of inflight jobs associated with the queue.
    /// The key is dynamically generated using the namespace of the queue.
    ///
    /// # Returns
    /// A `String` representing the Redis key for the inflight jobs set.
    pub fn inflight_jobs_set(&self) -> String {
        INFLIGHT_JOB_SET.replace("{queue}", &self.namespace)
    }

    /// Returns the Redis key for the hash storing job data associated with the queue.
    /// The key is dynamically generated using the namespace of the queue.
    ///
    /// # Returns
    /// A `String` representing the Redis key for the job data hash.
    pub fn job_data_hash(&self) -> String {
        JOB_DATA_HASH.replace("{queue}", &self.namespace)
    }

    /// Returns the Redis key for the set of scheduled jobs associated with the queue.
    /// The key is dynamically generated using the namespace of the queue.
    ///
    /// # Returns
    /// A `String` representing the Redis key for the scheduled jobs set.
    pub fn scheduled_jobs_set(&self) -> String {
        SCHEDULED_JOBS_SET.replace("{queue}", &self.namespace)
    }

    /// Returns the Redis key for the list of signals associated with the queue.
    /// The key is dynamically generated using the namespace of the queue.
    ///
    /// # Returns
    /// A `String` representing the Redis key for the signal list.
    pub fn signal_list(&self) -> String {
        SIGNAL_LIST.replace("{queue}", &self.namespace)
    }
}

/// The codec used by redis to encode and decode jobs
pub type RedisCodec<T> = Arc<
    Box<dyn Codec<RedisJob<T>, Vec<u8>, Error = apalis_core::error::Error> + Sync + Send + 'static>,
>;

/// Represents a [Storage] that uses Redis for storage.
pub struct RedisStorage<T> {
    conn: ConnectionManager,
    job_type: PhantomData<T>,
    scripts: RedisScript,
    controller: Controller,
    config: Config,
    codec: RedisCodec<T>,
}

impl<T> fmt::Debug for RedisStorage<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RedisStorage")
            .field("conn", &"ConnectionManager")
            .field("job_type", &std::any::type_name::<T>())
            .field("scripts", &self.scripts)
            .field("config", &self.config)
            .finish()
    }
}

impl<T> Clone for RedisStorage<T> {
    fn clone(&self) -> Self {
        Self {
            conn: self.conn.clone(),
            job_type: PhantomData,
            scripts: self.scripts.clone(),
            controller: self.controller.clone(),
            config: self.config.clone(),
            codec: self.codec.clone(),
        }
    }
}

impl<T: Serialize + DeserializeOwned> RedisStorage<T> {
    /// Start a new connection providing custom config
    pub fn new(conn: ConnectionManager, config: Config) -> Self {
        Self::new_with_codec(conn, config, JsonCodec)
    }
    /// Start a new connection providing custom config and a codec
    pub fn new_with_codec<C>(conn: ConnectionManager, config: Config, codec: C) -> Self
    where
        C: Codec<RedisJob<T>, Vec<u8>, Error = apalis_core::error::Error> + Sync + Send + 'static,
    {
        RedisStorage {
            conn,
            job_type: PhantomData,
            controller: Controller::new(),
            config,
            codec: Arc::new(Box::new(codec)),
            scripts: RedisScript {
                ack_job: redis::Script::new(include_str!("../lua/ack_job.lua")),
                push_job: redis::Script::new(include_str!("../lua/push_job.lua")),
                retry_job: redis::Script::new(include_str!("../lua/retry_job.lua")),
                enqueue_scheduled: redis::Script::new(include_str!(
                    "../lua/enqueue_scheduled_jobs.lua"
                )),
                get_jobs: redis::Script::new(include_str!("../lua/get_jobs.lua")),
                register_consumer: redis::Script::new(include_str!("../lua/register_consumer.lua")),
                kill_job: redis::Script::new(include_str!("../lua/kill_job.lua")),
                reenqueue_active: redis::Script::new(include_str!(
                    "../lua/reenqueue_active_jobs.lua"
                )),
                reenqueue_orphaned: redis::Script::new(include_str!(
                    "../lua/reenqueue_orphaned_jobs.lua"
                )),
                schedule_job: redis::Script::new(include_str!("../lua/schedule_job.lua")),
                vacuum: redis::Script::new(include_str!("../lua/vacuum.lua")),
            },
        }
    }

    /// Get current connection
    pub fn get_connection(&self) -> ConnectionManager {
        self.conn.clone()
    }

    /// Get the config used by the storage
    pub fn get_config(&self) -> &Config {
        &self.config
    }

    /// Get the underlying codec details
    pub fn get_codec(&self) -> &RedisCodec<T> {
        &self.codec
    }
}

impl<T: Serialize + DeserializeOwned + Sync + Send + Unpin + 'static> Backend<Request<T>>
    for RedisStorage<T>
{
    type Stream = BackendStream<RequestStream<Request<T>>>;

    type Layer = AckLayer<RedisStorage<T>, T>;

    fn common_layer(&self, worker_id: WorkerId) -> Self::Layer {
        AckLayer::new(self.clone(), worker_id)
    }

    fn poll(self, worker: WorkerId) -> Poller<Self::Stream> {
        let mut storage = self.clone();
        let controller = self.controller.clone();
        let config = self.config.clone();
        let stream: RequestStream<Request<T>> = Box::pin(
            self.stream_jobs(&worker, config.fetch_interval, config.buffer_size)
                .map_err(|e| Error::SourceError(e.into())),
        );

        let keep_alive = async move {
            loop {
                if let Err(e) = storage.keep_alive(&worker).await {
                    error!("Could not call keep_alive for Worker [{worker}]: {e}")
                }
                apalis_core::sleep(config.keep_alive).await;
            }
        }
        .boxed();
        let mut storage = self.clone();
        let enqueue_scheduled = async move {
            loop {
                if let Err(e) = storage.enqueue_scheduled(config.buffer_size).await {
                    error!("Could not call enqueue_scheduled: {e}")
                }
                apalis_core::sleep(config.enqueue_scheduled).await;
            }
        }
        .boxed();
        let heartbeat = async move {
            futures::join!(enqueue_scheduled, keep_alive);
        };
        Poller::new(BackendStream::new(stream, controller), heartbeat.boxed())
    }
}

impl<T: Sync> Ack<T> for RedisStorage<T> {
    type Acknowledger = TaskId;
    type Error = RedisError;
    async fn ack(
        &self,
        worker_id: &WorkerId,
        task_id: &Self::Acknowledger,
    ) -> Result<(), RedisError> {
        let mut conn = self.conn.clone();
        let ack_job = self.scripts.ack_job.clone();
        let inflight_set = format!("{}:{}", self.config.inflight_jobs_set(), worker_id);
        let done_jobs_set = &self.config.done_jobs_set();

        let now: i64 = Utc::now().timestamp();

        ack_job
            .key(inflight_set)
            .key(done_jobs_set)
            .arg(task_id.to_string())
            .arg(now)
            .invoke_async(&mut conn)
            .await
    }
}

impl<T: DeserializeOwned + Send + Unpin + Send + Sync + 'static> RedisStorage<T> {
    fn stream_jobs(
        &self,
        worker_id: &WorkerId,
        interval: Duration,
        buffer_size: usize,
    ) -> RequestStream<Request<T>> {
        let mut conn = self.conn.clone();
        let fetch_jobs = self.scripts.get_jobs.clone();
        let consumers_set = self.config.consumers_set();
        let active_jobs_list = self.config.active_jobs_list();
        let job_data_hash = self.config.job_data_hash();
        let inflight_set = format!("{}:{}", self.config.inflight_jobs_set(), worker_id);
        let signal_list = self.config.signal_list();
        let codec = self.codec.clone();
        let namespace = self.config.namespace.clone();
        Box::pin(try_stream! {
            loop {
                apalis_core::sleep(interval).await;
                let result = fetch_jobs
                    .key(&consumers_set)
                    .key(&active_jobs_list)
                    .key(&inflight_set)
                    .key(&job_data_hash)
                    .key(&signal_list)
                    .arg(buffer_size) // No of jobs to fetch
                    .arg(&inflight_set)
                    .invoke_async::<_, Vec<Value>>(&mut conn).await;
                match result {
                    Ok(jobs) => {
                        for job in jobs {
                            let request = deserialize_job(&job).map(|res| codec.decode(res)).transpose()?.map(Into::into).map(|mut req: Request<T>| {
                                req.insert(Namespace(namespace.clone()));
                                req
                            });
                            yield request
                        }
                    },
                    Err(e) => {
                        warn!("An error occurred during streaming jobs: {e}");
                    }
                }


            }
        })
    }
}

fn deserialize_job(job: &Value) -> Option<&Vec<u8>> {
    let job = match job {
        job @ Value::Data(_) => Some(job),
        Value::Bulk(val) => val.first(),
        _ => {
            error!(
                "Decoding Message Failed: {:?}",
                "unknown result type for next message"
            );
            None
        }
    };

    match job {
        Some(Value::Data(v)) => Some(v),
        None => None,
        _ => {
            error!("Decoding Message Failed: {:?}", "Expected Data(&Vec<u8>)");
            None
        }
    }
}

impl<T> RedisStorage<T> {
    async fn keep_alive(&mut self, worker_id: &WorkerId) -> Result<(), RedisError> {
        let mut conn = self.conn.clone();
        let register_consumer = self.scripts.register_consumer.clone();
        let inflight_set = format!("{}:{}", self.config.inflight_jobs_set(), worker_id);
        let consumers_set = self.config.consumers_set();

        let now: i64 = Utc::now().timestamp();

        register_consumer
            .key(consumers_set)
            .arg(now)
            .arg(inflight_set)
            .invoke_async(&mut conn)
            .await
    }
}

impl<T> Storage for RedisStorage<T>
where
    T: Serialize + DeserializeOwned + Send + 'static + Unpin + Sync,
{
    type Job = T;
    type Error = RedisError;
    type Identifier = TaskId;

    async fn push(&mut self, job: Self::Job) -> Result<TaskId, RedisError> {
        let mut conn = self.conn.clone();
        let push_job = self.scripts.push_job.clone();
        let job_data_hash = self.config.job_data_hash();
        let active_jobs_list = self.config.active_jobs_list();
        let signal_list = self.config.signal_list();
        let job_id = TaskId::new();
        let ctx = Context {
            attempts: 0,
            id: job_id.clone(),
        };
        let job = self
            .codec
            .encode(&RedisJob { ctx, job })
            .map_err(|e| (ErrorKind::IoError, "Encode error", e.to_string()))?;
        push_job
            .key(job_data_hash)
            .key(active_jobs_list)
            .key(signal_list)
            .arg(job_id.to_string())
            .arg(job)
            .invoke_async(&mut conn)
            .await?;
        Ok(job_id.clone())
    }

    async fn schedule(&mut self, job: Self::Job, on: i64) -> Result<TaskId, RedisError> {
        let mut conn = self.conn.clone();
        let schedule_job = self.scripts.schedule_job.clone();
        let job_data_hash = self.config.job_data_hash();
        let scheduled_jobs_set = self.config.scheduled_jobs_set();
        let job_id = TaskId::new();
        let ctx = Context {
            attempts: 0,
            id: job_id.clone(),
        };
        let job = RedisJob { job, ctx };
        let job = self
            .codec
            .encode(&job)
            .map_err(|e| (ErrorKind::IoError, "Encode error", e.to_string()))?;
        schedule_job
            .key(job_data_hash)
            .key(scheduled_jobs_set)
            .arg(job_id.to_string())
            .arg(job)
            .arg(on)
            .invoke_async(&mut conn)
            .await?;
        Ok(job_id.clone())
    }

    async fn len(&self) -> Result<i64, RedisError> {
        let mut conn = self.conn.clone();
        let all_jobs: i64 = redis::cmd("HLEN")
            .arg(&self.config.job_data_hash())
            .query_async(&mut conn)
            .await?;
        let done_jobs: i64 = redis::cmd("ZCOUNT")
            .arg(self.config.done_jobs_set())
            .arg("-inf")
            .arg("+inf")
            .query_async(&mut conn)
            .await?;
        Ok(all_jobs - done_jobs)
    }

    async fn fetch_by_id(&self, job_id: &TaskId) -> Result<Option<Request<Self::Job>>, RedisError> {
        let mut conn = self.conn.clone();
        let data: Value = redis::cmd("HMGET")
            .arg(&self.config.job_data_hash())
            .arg(job_id.to_string())
            .query_async(&mut conn)
            .await?;
        let job = deserialize_job(&data);
        match job {
            None => Err(RedisError::from((
                ErrorKind::ResponseError,
                "Invalid data returned by storage",
            ))),
            Some(bytes) => {
                let inner = self
                    .codec
                    .decode(bytes)
                    .map_err(|e| (ErrorKind::IoError, "Decode error", e.to_string()))?;
                Ok(Some(inner.into()))
            }
        }
    }
    async fn update(&self, job: Request<T>) -> Result<(), RedisError> {
        let job = job.try_into()?;
        let mut conn = self.conn.clone();
        let bytes = self
            .codec
            .encode(&job)
            .map_err(|e| (ErrorKind::IoError, "Encode error", e.to_string()))?;
        let _: i64 = redis::cmd("HSET")
            .arg(&self.config.job_data_hash())
            .arg(job.ctx.id.to_string())
            .arg(bytes)
            .query_async(&mut conn)
            .await?;
        Ok(())
    }

    async fn reschedule(&mut self, job: Request<T>, wait: Duration) -> Result<(), RedisError> {
        let mut conn = self.conn.clone();
        let schedule_job = self.scripts.schedule_job.clone();
        let job_id = job
            .get::<TaskId>()
            .cloned()
            .ok_or((ErrorKind::IoError, "Missing TaskId"))?;
        let worker_id = job
            .get::<WorkerId>()
            .cloned()
            .ok_or((ErrorKind::IoError, "Missing WorkerId"))?;
        let job = self
            .codec
            .encode(&(job.try_into()?))
            .map_err(|e| (ErrorKind::IoError, "Encode error", e.to_string()))?;
        let job_data_hash = self.config.job_data_hash();
        let scheduled_jobs_set = self.config.scheduled_jobs_set();
        let on: i64 = Utc::now().timestamp();
        let wait: i64 = wait
            .as_secs()
            .try_into()
            .map_err(|e: TryFromIntError| (ErrorKind::IoError, "Duration error", e.to_string()))?;
        let inflight_set = format!("{}:{}", self.config.inflight_jobs_set(), worker_id);
        let failed_jobs_set = self.config.failed_jobs_set();
        redis::cmd("SREM")
            .arg(inflight_set)
            .arg(job_id.to_string())
            .query_async(&mut conn)
            .await?;
        redis::cmd("ZADD")
            .arg(failed_jobs_set)
            .arg(on)
            .arg(job_id.to_string())
            .query_async(&mut conn)
            .await?;
        schedule_job
            .key(job_data_hash)
            .key(scheduled_jobs_set)
            .arg(job_id.to_string())
            .arg(job)
            .arg(on + wait)
            .invoke_async(&mut conn)
            .await
    }
    async fn is_empty(&self) -> Result<bool, RedisError> {
        self.len().map_ok(|res| res == 0).await
    }

    async fn vacuum(&self) -> Result<usize, RedisError> {
        let vacuum_script = self.scripts.vacuum.clone();
        let mut conn = self.conn.clone();

        vacuum_script
            .key(self.config.dead_jobs_set())
            .key(self.config.job_data_hash())
            .invoke_async(&mut conn)
            .await
    }
}

impl<T> RedisStorage<T> {
    /// Attempt to retry a job
    pub async fn retry(&mut self, worker_id: &WorkerId, task_id: &TaskId) -> Result<i32, RedisError>
    where
        T: Send + DeserializeOwned + Serialize + Unpin + Sync + 'static,
    {
        let mut conn = self.conn.clone();
        let retry_job = self.scripts.retry_job.clone();
        let inflight_set = format!("{}:{}", self.config.inflight_jobs_set(), worker_id);
        let scheduled_jobs_set = self.config.scheduled_jobs_set();
        let job_data_hash = self.config.job_data_hash();
        let job_fut = self.fetch_by_id(task_id);
        let failed_jobs_set = self.config.failed_jobs_set();
        let mut storage = self.clone();
        let now: i64 = Utc::now().timestamp();
        let res = job_fut.await?;
        match res {
            Some(job) => {
                let attempt = job.get::<Attempt>().cloned().unwrap_or_default();
                if attempt.current() >= self.config.max_retries {
                    redis::cmd("ZADD")
                        .arg(failed_jobs_set)
                        .arg(now)
                        .arg(task_id.to_string())
                        .query_async(&mut conn)
                        .await?;
                    storage.kill(worker_id, task_id).await?;
                    return Ok(1);
                }
                let job = self
                    .codec
                    .encode(&(job.try_into()?))
                    .map_err(|e| (ErrorKind::IoError, "Encode error", e.to_string()))?;

                let res: Result<i32, RedisError> = retry_job
                    .key(inflight_set)
                    .key(scheduled_jobs_set)
                    .key(job_data_hash)
                    .arg(task_id.to_string())
                    .arg(now)
                    .arg(job)
                    .invoke_async(&mut conn)
                    .await;
                match res {
                    Ok(count) => Ok(count),
                    Err(e) => Err(e),
                }
            }
            None => Err(RedisError::from((ErrorKind::ResponseError, "Id not found"))),
        }
    }

    /// Attempt to kill a job
    pub async fn kill(&mut self, worker_id: &WorkerId, task_id: &TaskId) -> Result<(), RedisError>
    where
        T: Send + DeserializeOwned + Serialize + Unpin + Sync + 'static,
    {
        let mut conn = self.conn.clone();
        let kill_job = self.scripts.kill_job.clone();
        let current_worker_id = format!("{}:{}", self.config.inflight_jobs_set(), worker_id);
        let job_data_hash = self.config.job_data_hash();
        let dead_jobs_set = self.config.dead_jobs_set();
        let fetch_job = self.fetch_by_id(task_id);
        let now: i64 = Utc::now().timestamp();
        let res = fetch_job.await?;
        match res {
            Some(job) => {
                let data = self
                    .codec
                    .encode(&job.try_into()?)
                    .map_err(|e| (ErrorKind::IoError, "Encode error", e.to_string()))?;
                kill_job
                    .key(current_worker_id)
                    .key(dead_jobs_set)
                    .key(job_data_hash)
                    .arg(task_id.to_string())
                    .arg(now)
                    .arg(data)
                    .invoke_async(&mut conn)
                    .await
            }
            None => Err(RedisError::from((ErrorKind::ResponseError, "Id not found"))),
        }
    }

    /// Required to add scheduled jobs to the active set
    pub async fn enqueue_scheduled(&mut self, count: usize) -> Result<usize, RedisError> {
        let enqueue_jobs = self.scripts.enqueue_scheduled.clone();
        let scheduled_jobs_set = self.config.scheduled_jobs_set();
        let active_jobs_list = self.config.active_jobs_list();
        let signal_list = self.config.signal_list();
        let now: i64 = Utc::now().timestamp();
        let res: Result<usize, _> = enqueue_jobs
            .key(scheduled_jobs_set)
            .key(active_jobs_list)
            .key(signal_list)
            .arg(now)
            .arg(count)
            .invoke_async(&mut self.conn)
            .await;
        match res {
            Ok(count) => Ok(count),
            Err(e) => Err(e),
        }
    }

    /// Re-enqueue some jobs that might be abandoned.
    pub async fn reenqueue_active(&mut self, job_ids: Vec<&TaskId>) -> Result<(), RedisError> {
        let mut conn = self.conn.clone();
        let reenqueue_active = self.scripts.reenqueue_active.clone();
        let inflight_set = self.config.inflight_jobs_set().to_string();
        let active_jobs_list = self.config.active_jobs_list();
        let signal_list = self.config.signal_list();

        reenqueue_active
            .key(inflight_set)
            .key(active_jobs_list)
            .key(signal_list)
            .arg(
                job_ids
                    .into_iter()
                    .map(|j| j.to_string())
                    .collect::<Vec<String>>(),
            )
            .invoke_async(&mut conn)
            .await
    }
    /// Re-enqueue some jobs that might be orphaned.
    pub async fn reenqueue_orphaned(
        &mut self,
        count: usize,
        dead_since: i64,
    ) -> Result<usize, RedisError> {
        let reenqueue_orphaned = self.scripts.reenqueue_orphaned.clone();
        let consumers_set = self.config.consumers_set();
        let active_jobs_list = self.config.active_jobs_list();
        let signal_list = self.config.signal_list();

        let res: Result<usize, RedisError> = reenqueue_orphaned
            .key(consumers_set)
            .key(active_jobs_list)
            .key(signal_list)
            .arg(dead_since)
            .arg(count)
            .invoke_async(&mut self.conn)
            .await;
        match res {
            Ok(count) => Ok(count),
            Err(e) => Err(e),
        }
    }
}

#[cfg(test)]
mod tests {
    use email_service::Email;
    use futures::StreamExt;

    use super::*;

    /// migrate DB and return a storage instance.
    async fn setup() -> RedisStorage<Email> {
        let redis_url = std::env::var("REDIS_URL").expect("No REDIS_URL is specified");
        // Because connections cannot be shared across async runtime
        // (different runtimes are created for each test),
        // we don't share the storage and tests must be run sequentially.
        let conn = connect(redis_url).await.unwrap();
        let storage = RedisStorage::new(conn, Config::default());
        storage
    }

    /// rollback DB changes made by tests.
    ///
    /// You should execute this function in the end of a test
    async fn cleanup(mut storage: RedisStorage<Email>, _worker_id: &WorkerId) {
        let _resp: String = redis::cmd("FLUSHDB")
            .query_async(&mut storage.conn)
            .await
            .expect("failed to Flushdb");
    }

    fn example_email() -> Email {
        Email {
            subject: "Test Subject".to_string(),
            to: "example@postgres".to_string(),
            text: "Some Text".to_string(),
        }
    }

    async fn consume_one(storage: &RedisStorage<Email>, worker_id: &WorkerId) -> Request<Email> {
        let mut stream = storage.stream_jobs(worker_id, std::time::Duration::from_secs(10), 1);
        stream
            .next()
            .await
            .expect("stream is empty")
            .expect("failed to poll job")
            .expect("no job is pending")
    }

    async fn register_worker_at(storage: &mut RedisStorage<Email>) -> WorkerId {
        let worker = WorkerId::new("test-worker");

        storage
            .keep_alive(&worker)
            .await
            .expect("failed to register worker");
        worker
    }

    async fn register_worker(storage: &mut RedisStorage<Email>) -> WorkerId {
        register_worker_at(storage).await
    }

    async fn push_email(storage: &mut RedisStorage<Email>, email: Email) {
        storage.push(email).await.expect("failed to push a job");
    }

    async fn get_job(storage: &mut RedisStorage<Email>, job_id: &TaskId) -> Request<Email> {
        storage
            .fetch_by_id(job_id)
            .await
            .expect("failed to fetch job by id")
            .expect("no job found by id")
    }

    #[tokio::test]
    async fn test_consume_last_pushed_job() {
        let mut storage = setup().await;
        push_email(&mut storage, example_email()).await;

        let worker_id = register_worker(&mut storage).await;

        let _job = consume_one(&mut storage, &worker_id).await;

        cleanup(storage, &worker_id).await;
    }

    #[tokio::test]
    async fn test_acknowledge_job() {
        let mut storage = setup().await;
        push_email(&mut storage, example_email()).await;

        let worker_id = register_worker(&mut storage).await;

        let job = consume_one(&mut storage, &worker_id).await;
        let job_id = &job.get::<Context>().unwrap().id;

        storage
            .ack(&worker_id, &job_id)
            .await
            .expect("failed to acknowledge the job");

        let _job = get_job(&mut storage, &job_id).await;
        cleanup(storage, &worker_id).await;
    }

    #[tokio::test]
    async fn test_kill_job() {
        let mut storage = setup().await;

        push_email(&mut storage, example_email()).await;

        let worker_id = register_worker(&mut storage).await;

        let job = consume_one(&mut storage, &worker_id).await;
        let job_id = &job.get::<Context>().unwrap().id;

        storage
            .kill(&worker_id, &job_id)
            .await
            .expect("failed to kill job");

        let _job = get_job(&mut storage, &job_id).await;

        cleanup(storage, &worker_id).await;
    }

    #[tokio::test]
    async fn test_heartbeat_renqueueorphaned_pulse_last_seen_6min() {
        let mut storage = setup().await;

        push_email(&mut storage, example_email()).await;

        let worker_id = register_worker_at(&mut storage).await;

        let _job = consume_one(&mut storage, &worker_id).await;
        storage
            .reenqueue_orphaned(5, 300)
            .await
            .expect("failed to reenqueue_orphaned");
        cleanup(storage, &worker_id).await;
    }

    #[tokio::test]
    async fn test_heartbeat_renqueueorphaned_pulse_last_seen_4min() {
        let mut storage = setup().await;

        push_email(&mut storage, example_email()).await;

        let worker_id = register_worker_at(&mut storage).await;

        let _job = consume_one(&mut storage, &worker_id).await;
        storage
            .reenqueue_orphaned(5, 300)
            .await
            .expect("failed to reenqueue_orphaned");

        cleanup(storage, &worker_id).await;
    }
}
