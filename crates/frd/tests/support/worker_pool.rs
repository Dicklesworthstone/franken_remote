//! Hold both configured pool workers without blocking the authority executor.
use asupersync::{cx::Cx, runtime::spawn_blocking::DrainedBlockingHandle, time::sleep};
use std::{
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
        mpsc::{Sender, channel},
    },
    time::{Duration, Instant},
};

pub struct HeldPool {
    release: Vec<Sender<()>>,
    jobs: Vec<DrainedBlockingHandle<()>>,
}
impl HeldPool {
    pub async fn new(cx: &Cx) -> Self {
        let entered = Arc::new(AtomicUsize::new(0));
        let mut held = Self {
            release: Vec::new(),
            jobs: Vec::new(),
        };
        for _ in 0..2 {
            let (send, receive) = channel();
            held.release.push(send);
            let entered = entered.clone();
            held.jobs.push(
                cx.spawn_blocking_drained(move |_| {
                    entered.fetch_add(1, Ordering::Release);
                    // Dropping HeldPool also releases on assertion/panic paths.
                    let _ = receive.recv_timeout(Duration::from_secs(5));
                })
                .unwrap(),
            );
        }
        let until = Instant::now() + Duration::from_secs(2);
        while entered.load(Ordering::Acquire) != 2 {
            assert!(Instant::now() < until, "both pool workers must enter");
            sleep(cx.timer_driver().unwrap().now(), Duration::from_millis(5)).await;
        }
        held
    }
    pub fn release(&mut self) {
        self.release.clear();
    }
    pub async fn finish(mut self) {
        self.release();
        for job in &mut self.jobs {
            job.join().await.unwrap();
        }
    }
}
