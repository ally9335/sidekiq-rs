use async_trait::async_trait;
use middleware::Chain;
use rand::{Rng, RngCore};
use serde::{Deserialize, Serialize};
use serde_json::Value as JsonValue;
use sha2::{Digest, Sha256};
use std::future::Future;
use std::marker::PhantomData;
use std::pin::Pin;
use std::sync::Arc;

pub mod periodic;

mod middleware;
mod processor;
mod redis;
mod scheduled;
mod stats;

// Re-export
pub use crate::redis::{
    with_custom_namespace, RedisConnection, RedisConnectionManager, RedisError, RedisPool,
};
pub use middleware::{ChainIter, ServerMiddleware, ServerResult};
pub use processor::{Processor, WorkFetcher};
pub use scheduled::Scheduled;
pub use stats::{Counter, StatsPublisher};

pub fn opts() -> EnqueueOpts {
    EnqueueOpts {
        queue: "default".into(),
        retry: true,
        unique_for: None,
        unique_hash_for_args: None,
    }
}

pub struct EnqueueOpts {
    queue: String,
    retry: bool,
    unique_for: Option<std::time::Duration>,
    unique_hash_for_args: Option<String>,
}

impl EnqueueOpts {
    pub fn queue<S: Into<String>>(self, queue: S) -> Self {
        Self {
            queue: queue.into(),
            ..self
        }
    }
    pub fn retry(self, retry: bool) -> Self {
        Self { retry, ..self }
    }

    pub fn unique_for(self, unique_for: std::time::Duration) -> Self {
        Self {
            unique_for: Some(unique_for),
            ..self
        }
    }

    pub fn unique_hash_for_args(self, unique_hash: String) -> Self {
        Self {
            unique_hash_for_args: Some(unique_hash),
            ..self
        }
    }

    fn create_job(
        &self,
        class: String,
        args: impl serde::Serialize,
    ) -> Result<Job, Box<dyn std::error::Error>> {
        let args = serde_json::to_value(args)?;

        // Ensure args are always wrapped in an array.
        let args = if args.is_array() {
            args
        } else {
            JsonValue::Array(vec![args])
        };

        Ok(Job {
            queue: self.queue.clone(),
            class,
            jid: new_jid(),
            created_at: chrono::Utc::now().timestamp() as f64,
            enqueued_at: None,
            retry: self.retry,
            args,

            // Make default eventually...
            error_message: None,
            failed_at: None,
            retry_count: None,
            retried_at: None,

            // Meta for enqueueing
            unique_for: self.unique_for,
            unique_hash_for_args: self.unique_hash_for_args.clone(),
        })
    }

    pub async fn perform_async(
        self,
        redis: &mut RedisPool,
        class: String,
        args: impl serde::Serialize,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let job = self.create_job(class, args)?;
        UnitOfWork::from_job(job).enqueue(redis).await?;
        Ok(())
    }

    pub async fn perform_in(
        self,
        redis: &mut RedisPool,
        class: String,
        duration: std::time::Duration,
        args: impl serde::Serialize,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let job = self.create_job(class, args)?;
        UnitOfWork::from_job(job).schedule(redis, duration).await?;
        Ok(())
    }
}

/// Helper function for enqueueing a worker into sidekiq.
/// This can be used to enqueue a job for a ruby sidekiq worker to process.
pub async fn perform_async(
    redis: &mut RedisPool,
    class: String,
    queue: String,
    args: impl serde::Serialize,
) -> Result<(), Box<dyn std::error::Error>> {
    opts().queue(queue).perform_async(redis, class, args).await
}

/// Helper function for enqueueing a worker into sidekiq.
/// This can be used to enqueue a job for a ruby sidekiq worker to process.
pub async fn perform_in(
    redis: &mut RedisPool,
    duration: std::time::Duration,
    class: String,
    queue: String,
    args: impl serde::Serialize,
) -> Result<(), Box<dyn std::error::Error>> {
    opts()
        .queue(queue)
        .perform_in(redis, class, duration, args)
        .await
}

fn new_jid() -> String {
    let mut bytes = [0u8; 12];
    rand::thread_rng().fill_bytes(&mut bytes);
    hex::encode(bytes)
}

pub struct WorkerOpts<Args, W: Worker<Args> + ?Sized> {
    queue: String,
    retry: bool,
    args: PhantomData<Args>,
    worker: PhantomData<W>,
    unique_for: Option<std::time::Duration>,
}

impl<Args, W> WorkerOpts<Args, W>
where
    W: Worker<Args>,
{
    pub fn new() -> Self {
        Self {
            queue: "default".into(),
            retry: true,
            args: PhantomData,
            worker: PhantomData,
            unique_for: None,
        }
    }

    pub fn retry(self, retry: bool) -> Self {
        Self { retry, ..self }
    }

    pub fn queue<S: Into<String>>(self, queue: S) -> Self {
        Self {
            queue: queue.into(),
            ..self
        }
    }

    pub fn unique_hash_for_args(&self, args: &Args) -> Result<String, Box<dyn std::error::Error>>
    where
        Args: serde::Serialize,
    {
        W::unique_hash_for_args(args)
    }

    pub fn unique_for(self, unique_for: std::time::Duration) -> Self {
        Self {
            unique_for: Some(unique_for),
            ..self
        }
    }

    #[allow(clippy::wrong_self_convention)]
    fn into_opts(&self) -> EnqueueOpts {
        self.into()
    }

    pub async fn perform_async(
        &self,
        redis: &mut RedisPool,
        args: Args,
    ) -> Result<(), Box<dyn std::error::Error>>
    where
        Args: serde::Serialize + Send + 'static,
    {
        let mut opts = self.into_opts();
        if self.unique_for.is_some() {
            let hash = self.unique_hash_for_args(&args)?;
            opts = opts.unique_hash_for_args(hash);
        }
        opts.perform_async(redis, W::class_name(), args).await
    }

    pub async fn perform_in(
        &self,
        redis: &mut RedisPool,
        duration: std::time::Duration,
        args: Args,
    ) -> Result<(), Box<dyn std::error::Error>>
    where
        Args: serde::Serialize + Send + 'static,
    {
        let mut opts = self.into_opts();
        if self.unique_for.is_some() {
            let hash = self.unique_hash_for_args(&args)?;
            opts = opts.unique_hash_for_args(hash);
        }
        opts.perform_in(redis, W::class_name(), duration, args)
            .await
    }
}

impl<Args, W: Worker<Args>> From<&WorkerOpts<Args, W>> for EnqueueOpts {
    fn from(opts: &WorkerOpts<Args, W>) -> Self {
        Self {
            retry: opts.retry,
            queue: opts.queue.clone(),
            unique_for: opts.unique_for,
            unique_hash_for_args: None,
        }
    }
}

impl<Args, W: Worker<Args>> Default for WorkerOpts<Args, W> {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
pub trait Worker<Args>: Send + Sync {
    /// Signal to WorkerRef to not attempt to modify the JsonValue args
    /// before calling the perform function. This is useful if the args
    /// are expected to be a `Vec<T>` that might be `len() == 1` or a
    /// single sized tuple `(T,)`.
    fn disable_argument_coercion(&self) -> bool {
        false
    }

    fn opts() -> WorkerOpts<Args, Self>
    where
        Self: Sized,
    {
        WorkerOpts::new()
    }

    // TODO: Make configurable through opts and make opts accessible to the
    // retry middleware through a Box<dyn Worker>.
    fn max_retries(&self) -> usize {
        25
    }

    fn unique_hash_for_args(args: &Args) -> Result<String, Box<dyn std::error::Error>>
    where
        Args: serde::Serialize,
    {
        let args_as_json_string: String = serde_json::to_string(args)?;
        Ok(format!("{:x}", Sha256::digest(&args_as_json_string)))
    }

    /// Derive a class_name from the Worker type to be used with sidekiq. By default
    /// this method will
    fn class_name() -> String
    where
        Self: Sized,
    {
        use heck::ToUpperCamelCase;
        let type_name = std::any::type_name::<Self>();
        let name = type_name.split("::").last().unwrap_or(type_name);
        name.to_upper_camel_case()
    }

    async fn perform_async(
        redis: &mut RedisPool,
        args: Args,
    ) -> Result<(), Box<dyn std::error::Error>>
    where
        Self: Sized,
        Args: Send + Sync + serde::Serialize + 'static,
    {
        Self::opts().perform_async(redis, args).await
    }

    async fn perform_in(
        redis: &mut RedisPool,
        duration: std::time::Duration,
        args: Args,
    ) -> Result<(), Box<dyn std::error::Error>>
    where
        Self: Sized,
        Args: Send + Sync + serde::Serialize + 'static,
    {
        Self::opts().perform_in(redis, duration, args).await
    }

    async fn perform(&self, args: Args) -> Result<(), Box<dyn std::error::Error>>;
}

// We can't store a Vec<Box<dyn Worker<Args>>>, because that will only work
// for a single arg type, but since any worker is JsonValue in and Result out,
// we can wrap that generic work in a callback that shares the same type.
// I'm sure this has a fancy name, but I don't know what it is.
#[derive(Clone)]
pub struct WorkerRef {
    #[allow(clippy::type_complexity)]
    work_fn: Arc<
        Box<
            dyn Fn(
                    JsonValue,
                )
                    -> Pin<Box<dyn Future<Output = Result<(), Box<dyn std::error::Error>>> + Send>>
                + Send
                + Sync,
        >,
    >,
    max_retries: usize,
}

async fn invoke_worker<Args, W>(args: JsonValue, worker: Arc<W>) -> ServerResult
where
    Args: Send + Sync + 'static,
    W: Worker<Args> + 'static,
    for<'de> Args: Deserialize<'de>,
{
    let args = if worker.disable_argument_coercion() {
        args
    } else {
        // Ensure any caller expecting to receive `()` will always work.
        if std::any::TypeId::of::<Args>() == std::any::TypeId::of::<()>() {
            JsonValue::Null
        } else {
            // If the value contains a single item Vec then
            // you can probably be sure that this is a single value item.
            // Otherwise, the caller can impl a tuple type.
            match args {
                JsonValue::Array(mut arr) if arr.len() == 1 => {
                    arr.pop().expect("value change after size check")
                }
                _ => args,
            }
        }
    };

    let args: Args = serde_json::from_value(args)?;
    worker.perform(args).await
}

impl WorkerRef {
    pub(crate) fn wrap<Args, W>(worker: Arc<W>) -> Self
    where
        Args: Send + Sync + 'static,
        W: Worker<Args> + 'static,
        for<'de> Args: Deserialize<'de>,
    {
        Self {
            work_fn: Arc::new(Box::new({
                let worker = worker.clone();
                move |args: JsonValue| {
                    let worker = worker.clone();
                    Box::pin(async move { invoke_worker(args, worker).await })
                }
            })),
            max_retries: worker.max_retries(),
        }
    }

    pub fn max_retries(&self) -> usize {
        self.max_retries
    }

    pub async fn call(&self, args: JsonValue) -> Result<(), Box<dyn std::error::Error>> {
        (Arc::clone(&self.work_fn))(args).await
    }
}

//
// {
//   "retry": true,
//   "queue": "yolo",
//   "class": "YoloWorker",
//   "args": [
//     {
//       "yolo": "hiiii"
//     }
//   ],
//   "jid": "f33f7063c6d7a4db0869289a",
//   "created_at": 1647119929.3788748,
//   "enqueued_at": 1647119929.378998
// }
//
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct Job {
    pub queue: String,
    pub args: JsonValue,
    pub retry: bool,
    pub class: String,
    pub jid: String,
    pub created_at: f64,
    pub enqueued_at: Option<f64>,
    pub failed_at: Option<f64>,
    pub error_message: Option<String>,
    pub retry_count: Option<usize>,
    pub retried_at: Option<f64>,

    #[serde(skip)]
    pub unique_for: Option<std::time::Duration>,
    pub unique_hash_for_args: Option<String>,
}

#[derive(Debug)]
pub struct UnitOfWork {
    queue: String,
    job: Job,
}

impl UnitOfWork {
    pub fn from_job(job: Job) -> Self {
        UnitOfWork {
            queue: format!("queue:{}", &job.queue),
            job,
        }
    }

    pub fn from_job_string(job_str: String) -> Result<Self, Box<dyn std::error::Error>> {
        let job: Job = serde_json::from_str(&job_str)?;
        Ok(Self::from_job(job))
    }

    pub async fn enqueue(&self, redis: &mut RedisPool) -> Result<(), Box<dyn std::error::Error>> {
        let mut redis = redis.get().await?;
        self.enqueue_direct(&mut *redis).await
    }

    async fn enqueue_direct(
        &self,
        redis: &mut RedisConnection,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let mut job = self.job.clone();
        job.enqueued_at = Some(chrono::Utc::now().timestamp() as f64);

        if let Some(ref duration) = job.unique_for {
            if let Some(ref args_hash) = job.unique_hash_for_args {
                // Check to see if this is unique for the given duration.
                // Even though SET k v NX EQ ttl isn't the best locking
                // mechanism, I think it's "good enough" to prove this out.
                let redis_key = format!(
                    "sidekiq:unique:{}:{}:{}",
                    &job.queue, &job.class, &args_hash
                );
                if let redis::RedisValue::Nil = redis
                    .set_nx_ex(redis_key, "".into(), duration.as_secs() as usize)
                    .await?
                {
                    // This job has already been enqueued. Do not submit it to redis.
                    return Ok(());
                }
            }
        }

        redis.sadd("queues".to_string(), job.queue.clone()).await?;

        redis
            .lpush(self.queue.clone(), serde_json::to_string(&job)?)
            .await?;
        Ok(())
    }

    pub async fn reenqueue(
        &mut self,
        redis: &mut RedisPool,
    ) -> Result<(), Box<dyn std::error::Error>> {
        if let Some(retry_count) = self.job.retry_count {
            redis
                .get()
                .await?
                .zadd(
                    "retry".to_string(),
                    serde_json::to_string(&self.job)?,
                    Self::retry_job_at(retry_count).timestamp(),
                )
                .await?;
        }

        Ok(())
    }

    fn retry_job_at(count: usize) -> chrono::DateTime<chrono::Utc> {
        let seconds_to_delay =
            count.pow(4) + 15 + (rand::thread_rng().gen_range(0..30) * (count + 1));

        chrono::Utc::now() + chrono::Duration::seconds(seconds_to_delay as i64)
    }

    pub async fn schedule(
        &mut self,
        redis: &mut RedisPool,
        duration: std::time::Duration,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let enqueue_at = chrono::Utc::now() + chrono::Duration::from_std(duration)?;

        redis
            .get()
            .await?
            .zadd(
                "schedule".to_string(),
                serde_json::to_string(&self.job)?,
                enqueue_at.timestamp(),
            )
            .await?;

        Ok(())
    }
}

#[cfg(test)]
mod test {
    use super::*;

    mod my {
        pub mod cool {
            pub mod workers {
                use super::super::super::super::*;

                pub struct TestModuleWorker;

                #[async_trait]
                impl Worker<()> for TestModuleWorker {
                    async fn perform(&self, _args: ()) -> ServerResult {
                        Ok(())
                    }
                }

                pub struct TestCustomClassNameWorker;

                #[async_trait]
                impl Worker<()> for TestCustomClassNameWorker {
                    async fn perform(&self, _args: ()) -> ServerResult {
                        Ok(())
                    }

                    fn class_name() -> String
                    where
                        Self: Sized,
                    {
                        "My::Cool::Workers::TestCustomClassNameWorker".to_string()
                    }
                }
            }
        }
    }

    #[tokio::test]
    async fn ignores_modules_in_ruby_worker_name() {
        assert_eq!(
            my::cool::workers::TestModuleWorker::class_name(),
            "TestModuleWorker".to_string()
        );
    }

    #[tokio::test]
    async fn supports_custom_class_name_for_workers() {
        assert_eq!(
            my::cool::workers::TestCustomClassNameWorker::class_name(),
            "My::Cool::Workers::TestCustomClassNameWorker".to_string()
        );
    }

    #[derive(Deserialize, Serialize, Debug)]
    struct TestArg {
        name: String,
        age: i32,
    }

    struct TestGenericWorker;
    #[async_trait]
    impl Worker<TestArg> for TestGenericWorker {
        async fn perform(&self, _args: TestArg) -> ServerResult {
            Ok(())
        }
    }

    struct TestMultiArgWorker;
    #[async_trait]
    impl Worker<(TestArg, TestArg)> for TestMultiArgWorker {
        async fn perform(&self, _args: (TestArg, TestArg)) -> ServerResult {
            Ok(())
        }
    }

    struct TestTupleArgWorker;
    #[async_trait]
    impl Worker<(TestArg,)> for TestTupleArgWorker {
        fn disable_argument_coercion(&self) -> bool {
            true
        }
        async fn perform(&self, _args: (TestArg,)) -> ServerResult {
            Ok(())
        }
    }

    struct TestVecArgWorker;
    #[async_trait]
    impl Worker<Vec<TestArg>> for TestVecArgWorker {
        fn disable_argument_coercion(&self) -> bool {
            true
        }
        async fn perform(&self, _args: Vec<TestArg>) -> ServerResult {
            Ok(())
        }
    }

    #[tokio::test]
    async fn can_have_a_vec_with_one_or_more_items() {
        // One item
        let worker = Arc::new(TestVecArgWorker);
        let wrap = Arc::new(WorkerRef::wrap(worker));
        let wrap = wrap.clone();
        let arg = serde_json::to_value(vec![TestArg {
            name: "test A".into(),
            age: 1337,
        }])
        .unwrap();
        wrap.call(arg).await.unwrap();

        // Multiple items
        let worker = Arc::new(TestVecArgWorker);
        let wrap = Arc::new(WorkerRef::wrap(worker));
        let wrap = wrap.clone();
        let arg = serde_json::to_value(vec![
            TestArg {
                name: "test A".into(),
                age: 1337,
            },
            TestArg {
                name: "test A".into(),
                age: 1337,
            },
        ])
        .unwrap();
        wrap.call(arg).await.unwrap();
    }

    #[tokio::test]
    async fn can_have_multiple_arguments() {
        let worker = Arc::new(TestMultiArgWorker);
        let wrap = Arc::new(WorkerRef::wrap(worker));
        let wrap = wrap.clone();
        let arg = serde_json::to_value((
            TestArg {
                name: "test A".into(),
                age: 1337,
            },
            TestArg {
                name: "test B".into(),
                age: 1336,
            },
        ))
        .unwrap();
        wrap.call(arg).await.unwrap();
    }

    #[tokio::test]
    async fn can_have_a_single_tuple_argument() {
        let worker = Arc::new(TestTupleArgWorker);
        let wrap = Arc::new(WorkerRef::wrap(worker));
        let wrap = wrap.clone();
        let arg = serde_json::to_value((TestArg {
            name: "test".into(),
            age: 1337,
        },))
        .unwrap();
        wrap.call(arg).await.unwrap();
    }

    #[tokio::test]
    async fn can_have_a_single_argument() {
        let worker = Arc::new(TestGenericWorker);
        let wrap = Arc::new(WorkerRef::wrap(worker));
        let wrap = wrap.clone();
        let arg = serde_json::to_value(TestArg {
            name: "test".into(),
            age: 1337,
        })
        .unwrap();
        wrap.call(arg).await.unwrap();
    }
}
