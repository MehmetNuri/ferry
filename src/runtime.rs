use std::future::Future;
use std::sync::OnceLock;
use tokio::runtime::Runtime;

pub fn runtime() -> &'static Runtime {
    static RUNTIME: OnceLock<Runtime> = OnceLock::new();
    RUNTIME.get_or_init(|| {
        tokio::runtime::Builder::new_multi_thread().enable_all().build().expect("Tokio runtime could not be started")
    })
}

pub async fn bg<F, T>(future: F) -> Result<T, String>
where
    F: Future<Output = Result<T, String>> + Send + 'static,
    T: Send + 'static,
{
    match runtime().spawn(future).await {
        Ok(result) => result,
        Err(error) => Err(error.to_string()),
    }
}
