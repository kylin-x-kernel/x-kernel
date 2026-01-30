// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use std::{
    future::Future,
    pin::Pin,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    task::{Context, Poll},
};

use futures::future;
use kpoll::PollSet;
use tokio::sync::Barrier;

struct WaitFuture {
    ps: Arc<PollSet>,
    ready: Arc<AtomicBool>,
}

impl Future for WaitFuture {
    type Output = ();

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        if self.ready.load(Ordering::SeqCst) {
            Poll::Ready(())
        } else {
            self.ps.register(cx.waker());
            Poll::Pending
        }
    }
}

impl WaitFuture {
    fn new(ps: Arc<PollSet>, ready: Arc<AtomicBool>) -> Self {
        Self { ps, ready }
    }
}

#[tokio::test]
async fn async_wake_single() {
    let ps = Arc::new(PollSet::new());
    let ready = Arc::new(AtomicBool::new(false));

    let f = WaitFuture::new(ps.clone(), ready.clone());

    let dispatch_irq = tokio::spawn(async move {
        ready.store(true, Ordering::SeqCst);
        ps.wake();
    });

    f.await;
    dispatch_irq.await.unwrap();
}

#[tokio::test]
async fn async_wake_many() {
    let ps = Arc::new(PollSet::new());
    let mut flags = Vec::new();
    let mut dispatch_irqs = Vec::new();
    let barrier = Arc::new(Barrier::new(66));
    for _ in 0..65 {
        let flag = Arc::new(AtomicBool::new(false));
        let b = barrier.clone();
        let f = WaitFuture::new(ps.clone(), flag.clone());
        let h = tokio::spawn(async move {
            b.wait().await;
            f.await;
        });
        flags.push(flag);
        dispatch_irqs.push(h);
    }
    barrier.wait().await;

    let mut ready: Vec<_> = Vec::new();
    let mut pending: Vec<_> = Vec::new();
    for (i, h) in dispatch_irqs.into_iter().enumerate() {
        if i % 2 == 0 {
            ready.push(h);
            flags[i].store(true, Ordering::SeqCst);
        } else {
            pending.push(h);
        }
    }
    ps.wake();
    future::try_join_all(ready).await.unwrap();

    for (i, f) in flags.iter().enumerate() {
        if i % 2 != 0 {
            f.store(true, Ordering::SeqCst);
        }
    }
    ps.wake();
    future::try_join_all(pending).await.unwrap();
}
