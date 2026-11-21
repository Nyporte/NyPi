//! The worker's abort reaches a running job (a user's shell command, a compaction), not only a
//! task (decision D54). Faux provider only.

use std::time::{Duration, Instant};

use nypi_ai::providers::faux::{FauxProvider, faux_model};
use nypi_core::config::Paths;
use nypi_core::runtime::{Runtime, RuntimeOptions, SessionMode};
use nypi_core::worker;

#[tokio::test]
async fn abort_stops_a_running_job() {
    let (home, work) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let runtime = Runtime::new(RuntimeOptions {
        cwd: work.path().to_path_buf(),
        paths: Some(Paths { home: home.path().to_path_buf() }),
        model: Some("faux/faux-1".into()),
        extra_model: Some(faux_model()),
        stream_fn: Some(FauxProvider::new(vec![]).stream_fn()),
        session: SessionMode::Ephemeral,
        ..Default::default()
    })
    .unwrap();
    let (worker, _events) = worker::spawn(runtime);
    assert!(!worker.abort(), "nothing to abort while idle");

    let mut job = {
        let worker = worker.clone();
        tokio::spawn(async move {
            worker
                .with(|runtime| {
                    Box::pin(async move {
                        let cancel = runtime.agent.new_abort_scope();
                        runtime.run_user_bash("sleep 30", false, cancel).await.unwrap()
                    })
                })
                .await
                .unwrap()
        })
    };
    let started = Instant::now();
    // The job starts on the worker's own task and opens its abort scope there; abort until it ends.
    let run = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            tokio::select! {
                run = &mut job => break run.unwrap(),
                _ = tokio::time::sleep(Duration::from_millis(20)) => { worker.abort(); }
            }
        }
    })
    .await
    .expect("the abort ends the job");
    assert!(run.cancelled);
    assert!(started.elapsed() < Duration::from_secs(10));
}
